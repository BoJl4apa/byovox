//! `byovox setup`: the interactive wizard that writes a working config file.
//!
//! Depends on `config::EXAMPLE` — the documented default file — and edits it in place, line
//! by line, so every comment in it survives into the file the user ends up with. Depends on
//! `check` for the probes: each answer is exercised by the very stage `byovox check` runs, so
//! the row the wizard prints is the row the user will see again. Produces a config file, then
//! loads it and hands off to `check::run`.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::check;
use crate::config::{self, Config};
use crate::hotkey::parse_chord;
use crate::lang::Lang;

/// The section header a line opens, if it opens one: `[polish]` → `polish`.
fn section_of(line: &str) -> Option<&str> {
    let t = line.trim();
    t.strip_prefix('[')?.strip_suffix(']')
}

/// Whether this line assigns `key` at the top level of its section. Deliberately anchored at
/// column zero and requiring the exact `key = ` spelling the example file uses: a commented
/// `# he = "he"` is documentation, not an assignment, and must never be edited in place.
fn assigns(line: &str, key: &str) -> bool {
    line.strip_prefix(key)
        .is_some_and(|rest| rest.starts_with(" = "))
}

/// `text` with `section.key` set to the already-rendered TOML `value`, and every comment,
/// blank line and ordering left exactly as it was.
///
/// A line-oriented edit rather than `toml::to_string` of a `Config`, because the file this
/// produces is also the documentation: a user who opens it later still has the paragraph
/// explaining every key they were never asked a question about.
///
/// Fails when the key is not in the section — the wizard would otherwise write a file that
/// silently drops an answer the user gave.
pub fn set_key(text: &str, section: &str, key: &str, value: &str) -> Result<String> {
    let mut current = "";
    let mut done = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(s) = section_of(line) {
            current = s;
        } else if !done && current == section && assigns(line, key) {
            out.push(format!("{key} = {value}"));
            done = true;
            continue;
        }
        out.push(line.to_string());
    }
    if !done {
        bail!("`{key}` is not a key of `[{section}]` in the example config");
    }
    Ok(join(out))
}

/// `text` with `entries` added at the end of `[section]`, before any blank line that
/// separates it from the next one.
///
/// For `[language.by_layout]`, which is an open-ended map: the example file defines no keys
/// under it at all, only a commented example, so there is nothing for `set_key` to replace.
pub fn append_to_section(text: &str, section: &str, entries: &[String]) -> Result<String> {
    if entries.is_empty() {
        return Ok(text.to_string());
    }
    let mut current = "";
    let mut placed = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        // The next header closes the section, so the entries go in just before it.
        if let Some(s) = section_of(line) {
            if !placed && current == section {
                place(&mut out, entries);
                placed = true;
            }
            current = s;
        }
        out.push(line.to_string());
    }
    // The section ran to the end of the file.
    if !placed && current == section {
        place(&mut out, entries);
        placed = true;
    }
    if !placed {
        bail!("`[{section}]` is not in the example config");
    }
    Ok(join(out))
}

/// Append `entries` to the section built so far, under its last real line rather than under
/// the blank line that separates it from the next section.
fn place(out: &mut Vec<String>, entries: &[String]) {
    let blanks = out.iter().rev().take_while(|l| l.trim().is_empty()).count();
    let at = out.len() - blanks;
    out.splice(at..at, entries.iter().cloned());
}

/// LF, and a trailing newline: `.gitattributes` pins the example file to LF, and `lines`
/// dropped the last one.
fn join(lines: Vec<String>) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// A TOML basic string. Only the two characters that would end or re-open the literal are
/// escaped; every value the wizard writes is a URL, a variable name, a path or an ISO code,
/// and each is validated before it gets here — control characters, which a basic string
/// forbids and this does not escape, by `reject_control_chars` at the question itself.
pub fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A TOML array of strings; `[]` when there is nothing in it, as the example file spells it.
fn toml_list(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
    format!("[{}]", inner.join(", "))
}

/// Where the wizard offers to keep a token when the environment variable naming it is unset.
const DEFAULT_KEY_FILE: &str = "~/.config/byovox/env";

/// End of input. The wizard is a conversation and has nothing to fall back on: `--yes` is
/// deliberately not a thing, because there is no answer it could invent for `stt.base_url`.
const NO_TERMINAL: &str = "setup needs a terminal — run `byovox config --init` and edit the file";

const ABORTED: &str = "aborted — nothing was written";

/// One question, one line of input: the wizard's whole input surface, so the flow can be
/// walked in a test with scripted answers instead of a keyboard.
pub trait Prompter {
    /// The line the user typed, its terminator removed, or `None` once input has ended.
    fn ask(&mut self, question: &str) -> Option<String>;
}

/// The real one: the question on stdout, the answer from stdin.
pub struct Console;

impl Prompter for Console {
    fn ask(&mut self, question: &str) -> Option<String> {
        print!("{question} ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
            // Said out loud rather than folded into "input ended": a stdin that cannot be
            // read is a different fault from a closed one, and the abort below would
            // otherwise blame a missing terminal for it.
            Err(e) => {
                eprintln!("reading stdin: {e}");
                None
            }
        }
    }
}

/// The live half: `byovox check`'s own stages, run against the config the answers so far
/// describe. A test supplies scripted verdicts instead of a network.
pub trait Probe {
    /// Print the `stt` row for this config; `true` when it passed.
    fn stt(&mut self, cfg: &Config) -> bool;
    /// Print the `polish` row for this config; `true` when it passed.
    fn polish(&mut self, cfg: &Config) -> bool;
}

/// The stages themselves.
pub struct Stages;

impl Probe for Stages {
    fn stt(&mut self, cfg: &Config) -> bool {
        check::probe_stt(cfg)
    }
    fn polish(&mut self, cfg: &Config) -> bool {
        check::probe_polish(cfg)
    }
}

/// Every question the wizard asks, in the order it asks them. Everything else in the config
/// file keeps the default the example documents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Answers {
    pub stt_base_url: String,
    pub stt_api_key_env: String,
    pub stt_api_key_file: String,
    pub polish_enabled: bool,
    pub polish_base_url: String,
    pub polish_model: String,
    pub polish_api_key_env: String,
    pub polish_api_key_file: String,
    pub hotkey_key: String,
    pub candidates: Vec<String>,
    pub by_layout: Vec<(String, String)>,
    pub capture_log: bool,
}

/// Every `[section]`, key and rendered value the answers set. The single list of keys the
/// wizard writes: a drift test walks it against `EXAMPLE`, so a key renamed in the documented
/// file fails the build instead of making the wizard drop an answer at runtime.
fn edits(a: &Answers) -> Vec<(&'static str, &'static str, String)> {
    let mut v = vec![
        ("stt", "base_url", toml_string(&a.stt_base_url)),
        ("stt", "api_key_env", toml_string(&a.stt_api_key_env)),
        ("stt", "api_key_file", toml_string(&a.stt_api_key_file)),
        ("language", "candidates", toml_list(&a.candidates)),
        ("polish", "enabled", a.polish_enabled.to_string()),
        ("hotkey", "key", toml_string(&a.hotkey_key)),
        ("capture_log", "enabled", a.capture_log.to_string()),
    ];
    // Nothing was asked about a gateway that will not be called, so its keys keep the empty
    // defaults the example already documents.
    if a.polish_enabled {
        v.extend([
            ("polish", "base_url", toml_string(&a.polish_base_url)),
            ("polish", "model", toml_string(&a.polish_model)),
            ("polish", "api_key_env", toml_string(&a.polish_api_key_env)),
            (
                "polish",
                "api_key_file",
                toml_string(&a.polish_api_key_file),
            ),
        ]);
    }
    v
}

/// The config file these answers describe: the documented example with each answer set in
/// place. Every key nobody was asked about keeps its default *and* its paragraph.
pub fn render(a: &Answers) -> Result<String> {
    let mut text = config::EXAMPLE.to_string();
    for (section, key, value) in edits(a) {
        text = set_key(&text, section, key, &value)?;
    }
    let entries: Vec<String> = a
        .by_layout
        .iter()
        .map(|(layout, lang)| format!("{layout} = {}", toml_string(lang)))
        .collect();
    append_to_section(&text, "language.by_layout", &entries)
}

/// An endpoint byovox will post to: an absolute http(s) URL naming a host. Anything else
/// would reach the stage as a transport error naming nothing the user could act on.
fn parse_base_url(s: &str) -> Result<String, String> {
    let url = s.trim();
    if url.is_empty() {
        return Err("required: byovox has no server it could guess".into());
    }
    reject_control_chars(url)?;
    let host = url
        .split_once("://")
        .filter(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        })
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .ok_or_else(|| format!("`{url}` is not an http:// or https:// URL"))?;
    if host.is_empty() {
        return Err(format!("`{url}` names no host"));
    }
    Ok(url.trim_end_matches('/').to_string())
}

/// The longest answer read as a variable name. Real ones are short: `OPENAI_API_KEY` is 14,
/// and `GOOGLE_APPLICATION_CREDENTIALS`, about the longest in circulation, is 30.
const MAX_ENV_NAME: usize = 32;

/// Past this length a legal identifier has to *look* like a name to be read as one. Below it,
/// `key` and `_x1` are perfectly ordinary answers and nothing about them suggests a secret.
const NAME_SHAPE_FROM: usize = 20;

/// Prefixes that begin a credential and nothing else.
///
/// Case-sensitive, as the issuers spell them: matching `Bearer` case-insensitively would
/// refuse `BEARER_TOKEN`, which is a variable name somebody really might choose.
const SECRET_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "github_pat_",
    "sk-",
    "sk_",
    "xox",
    "AKIA",
    "AIza",
    "Bearer",
];

/// Whether every letter in `name` is upper case — `OPENAI_API_KEY` yes, `AIzaSyD…` no. True
/// for a name with no letters at all, which cannot happen here: the first character is always
/// a letter or `_`.
fn all_letters_upper(name: &str) -> bool {
    name.chars()
        .filter(char::is_ascii_alphabetic)
        .all(|c| c.is_ascii_uppercase())
}

/// Whether an answer that is a legal identifier is a credential rather than a variable name.
///
/// Positive shape rules, not a length threshold. The threshold version (`len() > 40`) let a
/// 40-character GitHub PAT straight through — the length its own comment cited: `ghp_` plus 36
/// alphanumerics is a legal identifier, contains an underscore, and is not bare hex, so every
/// arm was false and the token was written into the config file and printed back.
///
/// What separates the two populations is shape, not size. A name is short, and either
/// SCREAMING_SNAKE or carrying an underscore. A credential is long, mixed-case, often bare hex,
/// and frequently starts with an issuer's prefix — the arm that catches `AKIAIOSFODNN7EXAMPLE`,
/// which is 20 upper-case characters and would otherwise read as a perfectly good name.
///
/// Best effort, and knowingly so: nothing separates a 20-character name from a 20-character
/// key with certainty. This errs towards refusing, and the refusal explains itself.
fn looks_like_a_secret(name: &str) -> bool {
    if name.len() > MAX_ENV_NAME {
        return true;
    }
    if SECRET_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    if name.len() < NAME_SHAPE_FROM {
        return false;
    }
    // Long enough to be a key: bare hex is one, and anything without a name's own shape —
    // an underscore, or letters that are all upper case — is treated as one.
    name.chars().all(|c| c.is_ascii_hexdigit()) || (!name.contains('_') && !all_letters_upper(name))
}

/// The *name* of an environment variable, never a token. `[A-Za-z_][A-Za-z0-9_]*`, and refused
/// outright when it looks like a key instead.
///
/// Not fussiness: an accepted answer is written into the file `docs/config.example.toml`
/// promises never holds a secret, and echoed into a `check` FAIL row — the surface
/// `check::strip_body` and `config::redact_userinfo` exist to keep credentials out of, because
/// those rows get pasted into bug reports. Neither error echoes the answer back, for the same
/// reason. Empty = the endpoint needs no Authorization header.
fn parse_env_name(s: &str) -> Result<String, String> {
    let name = s.trim();
    if name.is_empty() {
        return Ok(String::new());
    }
    let mut chars = name.chars();
    let shaped = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !shaped {
        return Err(
            "that is not an environment variable name — a letter or `_`, then letters, digits \
             and `_`, e.g. `BYOVOX_API_KEY`. This is a name, not the secret."
                .into(),
        );
    }
    if looks_like_a_secret(name) {
        return Err(
            "this looks like a secret, not a variable name. byovox reads the token itself from \
             the environment or a key file, never from this prompt — give the NAME of the \
             variable that holds it, e.g. `BYOVOX_API_KEY`."
                .into(),
        );
    }
    Ok(name.to_string())
}

/// Refuse an answer carrying a control character.
///
/// TOML basic strings forbid every control character, and `toml_string` escapes only `\` and
/// `"` — so a pasted value with an interior `\x0b` in it used to survive every parser, reach
/// `render`, and fail the parse at the very end of the run, taking nine answered questions
/// with it and blaming the wizard for the user's paste. Caught at the question instead, which
/// is the one place it can still be retyped.
fn reject_control_chars(s: &str) -> Result<(), String> {
    if s.chars().any(char::is_control) {
        return Err(
            "that answer contains a control character — retype it rather than pasting".into(),
        );
    }
    Ok(())
}

/// The key file, or the default on an empty line.
fn parse_key_file(s: &str) -> Result<String, String> {
    let path = s.trim();
    if path.is_empty() {
        return Ok(DEFAULT_KEY_FILE.to_string());
    }
    reject_control_chars(path)?;
    Ok(path.to_string())
}

/// A non-empty answer, trimmed: for the questions with no default and nothing to validate.
fn parse_required(s: &str) -> Result<String, String> {
    let v = s.trim();
    reject_control_chars(v)?;
    if v.is_empty() {
        Err("required".into())
    } else {
        Ok(v.to_string())
    }
}

/// `en, ru` → the `language.candidates` list. Empty is the answer that sends no field at all.
fn parse_candidates(s: &str) -> Result<Vec<String>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(parse_code)
        .collect()
}

/// `he=he, ru=ru` → the `language.by_layout` entries. A layout named twice is refused rather
/// than written, since TOML would reject the duplicate key the wizard produced.
fn parse_by_layout(s: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (layout, lang) = pair
            .split_once('=')
            .ok_or_else(|| format!("`{pair}` is not a `layout=language` pair, e.g. `he=he`"))?;
        let layout = parse_code(layout.trim())?;
        let lang = parse_code(lang.trim())?;
        if out.iter().any(|(l, _)| *l == layout) {
            return Err(format!("`{layout}` is mapped twice"));
        }
        out.push((layout, lang));
    }
    Ok(out)
}

/// One ISO 639-1 code, by the same rule `lang::LanguagePolicy` applies at load.
fn parse_code(s: &str) -> Result<String, String> {
    Lang::parse(s)
        .map(|l| l.to_string())
        .ok_or_else(|| format!("`{s}` is not an ISO 639-1 code (two lowercase letters, e.g. `en`)"))
}

fn parse_yes_no(s: &str, default: bool) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "" => Ok(default),
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        other => Err(format!("`{other}`: answer y or n")),
    }
}

/// What to do about a stage whose probe just failed.
enum AfterFailure {
    Retry,
    Keep,
    Abort,
}

/// One question, re-asked until the answer parses; the parse error is printed in between.
/// End of input is fatal — there is no terminal left to correct anything on.
fn ask<T>(
    p: &mut dyn Prompter,
    question: &str,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<T> {
    loop {
        let Some(answer) = p.ask(question) else {
            bail!(NO_TERMINAL)
        };
        match parse(&answer) {
            Ok(v) => return Ok(v),
            Err(e) => println!("  {e}"),
        }
    }
}

/// The three ways out of a failed probe. "Keep anyway" exists because a server that is merely
/// not running yet is a perfectly good answer to write down.
fn after_failure(p: &mut dyn Prompter) -> Result<AfterFailure> {
    ask(p, "  retry, keep anyway, or abort? [R/k/a]", |s| {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "r" | "retry" => Ok(AfterFailure::Retry),
            "k" | "keep" => Ok(AfterFailure::Keep),
            "a" | "abort" => Ok(AfterFailure::Abort),
            other => Err(format!("`{other}`: answer r, k or a")),
        }
    })
}

/// The `api_key_env` / `api_key_file` pair for one stage. The file question is asked only
/// when the variable is unset here, because that is the case where naming it alone would send
/// the request out unauthenticated — or, since `check` refuses that, fail the probe.
fn ask_token(p: &mut dyn Prompter) -> Result<(String, String)> {
    let env = ask(
        p,
        "  Bearer token — the NAME of an environment variable holding it \
         (empty = no Authorization header) []",
        parse_env_name,
    )?;
    if env.is_empty() || config::resolve_token(&env, "").is_some() {
        return Ok((env, String::new()));
    }
    println!("  {env} is not set in this environment.");
    println!(
        "  byovox can read it from a KEY=VALUE file instead: one line reading `{env}=<token>`."
    );
    println!("  That file holds a live credential in plain text and byovox neither checks nor");
    println!("  changes its permissions — keep it readable by you alone (Windows: Properties >");
    println!("  Security, remove the inherited groups; Linux/macOS: chmod 600).");
    let file = ask(
        p,
        &format!("  key file [{DEFAULT_KEY_FILE}]"),
        parse_key_file,
    )?;
    Ok((env, file))
}

/// The config the answers so far describe — rendered to the file's own text and parsed back,
/// which is the only construction that cannot drift from what lands on disk.
///
/// A probe built by hand from `Answers` would be a second encoding of the same answers, free
/// to disagree with the file on any key nobody was asked about. This one cannot: it *is* the
/// file, minus the questions still to come.
fn partial_config(a: &Answers) -> Result<Config> {
    toml::from_str(&render(a)?).context("the file the wizard produced is not valid TOML")
}

fn ask_stt(p: &mut dyn Prompter, probe: &mut dyn Probe, a: &mut Answers) -> Result<()> {
    loop {
        println!("\nWhere is your speech-to-text server?");
        a.stt_base_url = ask(
            p,
            "  OpenAI-compatible base URL, e.g. http://127.0.0.1:8770/v1 (required)",
            parse_base_url,
        )?;
        let (env, file) = ask_token(p)?;
        a.stt_api_key_env = env;
        a.stt_api_key_file = file;
        println!("  transcribing a second of silence...");
        if probe.stt(&partial_config(a)?) {
            return Ok(());
        }
        match after_failure(p)? {
            AfterFailure::Retry => continue,
            AfterFailure::Keep => return Ok(()),
            AfterFailure::Abort => bail!(ABORTED),
        }
    }
}

fn ask_polish(p: &mut dyn Prompter, probe: &mut dyn Probe, a: &mut Answers) -> Result<()> {
    println!("\nPolish turns a raw transcript into punctuated text with the fillers removed.");
    println!("If it ever fails, the raw transcript is typed instead.");
    a.polish_enabled = ask(
        p,
        "  Clean up transcripts with a language model? [Y/n]",
        |s| parse_yes_no(s, true),
    )?;
    if !a.polish_enabled {
        return Ok(());
    }
    loop {
        a.polish_base_url = ask(
            p,
            "  Chat-completions base URL, e.g. http://127.0.0.1:4000/v1 (required)",
            parse_base_url,
        )?;
        a.polish_model = ask(
            p,
            "  Model name, or the alias your gateway serves (required)",
            parse_required,
        )?;
        let (env, file) = ask_token(p)?;
        a.polish_api_key_env = env;
        a.polish_api_key_file = file;
        println!("  polishing a sample sentence...");
        if probe.polish(&partial_config(a)?) {
            return Ok(());
        }
        match after_failure(p)? {
            AfterFailure::Retry => continue,
            AfterFailure::Keep => return Ok(()),
            AfterFailure::Abort => bail!(ABORTED),
        }
    }
}

/// Every question, in order. Nothing is written and nothing outside this process is touched:
/// a `Ctrl+C` or an abort here leaves the machine exactly as it was.
pub fn interview(p: &mut dyn Prompter, probe: &mut dyn Probe) -> Result<Answers> {
    let mut a = Answers::default();
    ask_stt(p, probe, &mut a)?;
    ask_polish(p, probe, &mut a)?;

    println!("\nThe key you hold down to dictate.");
    a.hotkey_key = ask(
        p,
        "  Push-to-talk key — a key name or a chord, e.g. Insert or ControlLeft+ShiftLeft+Z \
         [ControlRight]",
        |s| {
            let name = match s.trim() {
                "" => "ControlRight",
                given => given,
            };
            parse_chord(name).map(|_| name.to_string())
        },
    )?;

    println!("\nLanguages. byovox lets the server detect by default.");
    a.candidates = ask(
        p,
        "  Constrain auto-detection to these languages (comma-separated ISO codes, \
         empty = server default) []",
        parse_candidates,
    )?;
    a.by_layout = ask(
        p,
        "  Map keyboard layouts to an explicit language? e.g. he=he (empty = none) []",
        parse_by_layout,
    )?;

    println!("\nThe capture log keeps every dictation on this machine — the WAV plus a row of");
    println!("text — so you can score your own setup later; anything older than keep_days (30)");
    println!("is pruned automatically.");
    a.capture_log = ask(
        p,
        "  Keep a local copy of every dictation (audio + text) for your own re-scoring? [y/N]",
        |s| parse_yes_no(s, false),
    )?;
    Ok(a)
}

/// The overwrite question, in one wording wherever the collision is noticed.
fn ask_overwrite(p: &mut dyn Prompter, path: &Path) -> Result<bool> {
    ask(
        p,
        &format!("{} already exists. Overwrite it? [y/N]", path.display()),
        |s| parse_yes_no(s, false),
    )
}

/// Refuse to overwrite a config the user already has, unless they say so at a prompt naming
/// the file: the answers here are a small fraction of what a lived-in file holds.
///
/// Returns whether they granted it, so the write does not ask a second time about a file they
/// have already agreed to replace.
fn confirm_overwrite(p: &mut dyn Prompter, path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if !ask_overwrite(p, path)? {
        bail!("left {} alone", path.display());
    }
    Ok(true)
}

/// Write the answers, and refuse to replace a config that appeared while they were being
/// given. `granted` is the answer to the question asked before the interview.
///
/// `create_new` is the check and the write in one operation. An `exists()` before a
/// `fs::write` is a window, and this wizard holds that window open for the length of an
/// interview plus two network round trips — long enough for `byovox config --init` in another
/// terminal, or a second wizard, to land inside it. A newcomer's file is not something to lose
/// to a race, so the collision re-asks by name and only an explicit `y` goes through.
fn write_config(p: &mut dyn Prompter, path: &Path, text: &str, granted: bool) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let failed = || format!("writing {}", path.display());
    if !granted {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut f) => return f.write_all(text.as_bytes()).with_context(failed),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                println!(
                    "\n  a config appeared at {} while setup was running.",
                    path.display()
                );
                if !ask_overwrite(p, path)? {
                    bail!(
                        "left {} alone — your answers were not written",
                        path.display()
                    );
                }
            }
            Err(e) => return Err(e).with_context(failed),
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(failed)?;
    f.write_all(text.as_bytes()).with_context(failed)
}

/// The wizard. `false` when the closing `check` failed: a file was written, but it is not one
/// byovox can dictate with yet. The exit code that goes with that is `main`'s to choose, as it
/// already is for `check` — a public function of the library must stay callable by a test, by
/// the daemon, or by a tray item that has no business ending the process.
pub fn run(path: &Path) -> Result<bool> {
    println!("byovox setup — a few questions, each answer probed as you give it.");
    println!("Enter takes the default in brackets; Ctrl+C stops without writing anything.");
    println!("Config file: {}", path.display());
    let mut p = Console;
    let granted = confirm_overwrite(&mut p, path)?;
    let answers = interview(&mut p, &mut Stages)?;
    let text = render(&answers)?;
    // Parsed before it is written, so a wizard that produced junk says so instead of leaving
    // the junk on disk. `load` below is the one that validates.
    toml::from_str::<Config>(&text).context("the file the wizard produced is not valid TOML")?;
    println!();
    // No `check::warn_cleartext` call here. The `check` this function ends on prints those
    // rows a few seconds later, and nothing between here and there is a decision they could
    // inform — the file is written either way — so saying it twice is only noise.
    write_config(&mut p, path, &text, granted)?;
    println!("wrote {}\n", path.display());
    let cfg = config::load(path)?;
    Ok(check::run(&cfg, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EXAMPLE};

    #[test]
    fn setting_a_key_edits_only_that_line_and_keeps_every_comment() {
        let out = set_key(EXAMPLE, "stt", "base_url", "\"http://127.0.0.1:8770/v1\"").unwrap();
        assert!(out.contains("\nbase_url = \"http://127.0.0.1:8770/v1\"\n"));
        // The paragraph documenting the key it just set is still there.
        assert!(out.contains("# OpenAI-compatible base URL; byovox posts to"));
        assert_eq!(
            EXAMPLE.lines().count(),
            out.lines().count(),
            "an in-place edit adds and removes no lines"
        );
        assert_eq!(
            EXAMPLE.lines().filter(|l| l.starts_with('#')).count(),
            out.lines().filter(|l| l.starts_with('#')).count()
        );
    }

    /// `base_url`, `model`, `enabled`, `api_key_env` and `api_key_file` each appear in more
    /// than one section: an edit that ignored the header would set the wrong endpoint.
    #[test]
    fn a_key_of_the_same_name_in_another_section_is_untouched() {
        let out = set_key(EXAMPLE, "polish", "base_url", "\"http://p:4000/v1\"").unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.polish.base_url, "http://p:4000/v1");
        assert_eq!(cfg.stt.base_url, "");

        let out = set_key(EXAMPLE, "capture_log", "enabled", "true").unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert!(cfg.capture_log.enabled);
        assert!(cfg.polish.enabled, "the polish flag is a different line");
    }

    /// The commented `# he = "he"` is documentation. Editing it in place would both destroy
    /// the example and leave the entry under the wrong heading.
    #[test]
    fn a_commented_example_line_is_not_an_assignment() {
        assert!(!assigns("# he = \"he\"", "he"));
        assert!(!assigns("#base_url = \"\"", "base_url"));
        assert!(assigns("base_url = \"\"", "base_url"));
        // A longer key that merely starts with the one asked for.
        assert!(!assigns("base_url_2 = \"\"", "base_url"));
        assert!(!assigns("timeout_s = 30", "timeout"));
    }

    #[test]
    fn a_key_that_is_not_in_the_section_is_refused() {
        let e = set_key(EXAMPLE, "stt", "no_such_key", "1").unwrap_err();
        assert!(e.to_string().contains("no_such_key"), "{e}");
        // Right key, wrong section: `hotkey.key` is not a `[stt]` key.
        assert!(set_key(EXAMPLE, "stt", "key", "\"Insert\"").is_err());
    }

    #[test]
    fn layout_entries_land_under_the_map_and_nowhere_else() {
        let out = append_to_section(
            EXAMPLE,
            "language.by_layout",
            &["he = \"he\"".into(), "ru = \"ru\"".into()],
        )
        .unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            cfg.language.by_layout.get("he").map(String::as_str),
            Some("he")
        );
        assert_eq!(
            cfg.language.by_layout.get("ru").map(String::as_str),
            Some("ru")
        );
        // The commented example survives, and the entries follow it directly rather than
        // being pushed under the blank line that ends the section.
        assert!(
            out.contains("# he = \"he\"\nhe = \"he\"\nru = \"ru\"\n\n[polish]"),
            "{out}"
        );

        // Nothing to add changes nothing.
        assert_eq!(
            append_to_section(EXAMPLE, "language.by_layout", &[]).unwrap(),
            EXAMPLE
        );
        assert!(append_to_section(EXAMPLE, "no.such.section", &["a = 1".into()]).is_err());
    }

    /// The whole point of editing the example rather than serialising a `Config`: the result
    /// is still a file byovox loads *and* still the documentation for every key.
    #[test]
    fn an_edited_example_round_trips_through_the_config_loader() {
        let out = set_key(EXAMPLE, "stt", "base_url", "\"http://127.0.0.1:8770/v1\"").unwrap();
        let out = set_key(&out, "polish", "enabled", "false").unwrap();
        let dir =
            std::env::temp_dir().join(format!("byovox-setup-{}-roundtrip", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, &out).unwrap();
        let cfg = crate::config::load(&path).expect("the edited example validates");
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:8770/v1");
        assert!(!cfg.polish.enabled);
        // Every other key is still at the default the example documents.
        assert_eq!(cfg.hotkey.key, "ControlRight");
        assert_eq!(cfg.stt.no_speech_threshold, 0.3);
        // And the comments came with it.
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# byovox configuration.")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_value_cannot_break_out_of_its_string_literal() {
        assert_eq!(toml_string("http://x/v1"), "\"http://x/v1\"");
        assert_eq!(toml_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(toml_string(r"C:\keys\env"), r#""C:\\keys\\env""#);
        // And what it produces is what TOML reads back.
        let out = set_key(EXAMPLE, "stt", "api_key_file", &toml_string(r"C:\keys\env")).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.stt.api_key_file, r"C:\keys\env");
    }

    /// The wizard places each answer by matching `key = ` inside a section. A key renamed in
    /// the documented file, or one that turns up twice, would have it write to the wrong line
    /// or refuse outright — so the example file cannot drift away from the wizard.
    #[test]
    fn every_key_the_wizard_writes_is_settable_exactly_once_in_the_example() {
        // Polish on, so the keys that are only written when it is enabled are covered too.
        let a = Answers {
            polish_enabled: true,
            ..Default::default()
        };
        let written = edits(&a);
        assert!(written.len() >= 11, "{written:?}");
        for (section, key, _) in &written {
            let mut current = "";
            let mut hits = 0;
            for line in EXAMPLE.lines() {
                if let Some(s) = section_of(line) {
                    current = s;
                } else if current == *section && assigns(line, key) {
                    hits += 1;
                }
            }
            assert_eq!(hits, 1, "`{key}` under `[{section}]` in the example file");
        }
        // The map the wizard appends to exists and ships empty, or the entries would belong
        // in `set_key`'s hands rather than `append_to_section`'s.
        assert!(EXAMPLE.contains("\n[language.by_layout]\n"));
        assert!(Config::default().language.by_layout.is_empty());
    }

    /// A URL the request would fail on for a reason naming nothing the user could act on is
    /// caught while they are still looking at the question.
    #[test]
    fn only_an_absolute_http_url_with_a_host_is_accepted() {
        for good in [
            "http://127.0.0.1:8770/v1",
            "https://api.example.com/v1",
            "HTTP://10.0.0.5:8770/v1",
            "  http://host:8770/v1  ",
        ] {
            assert!(parse_base_url(good).is_ok(), "{good}");
        }
        assert_eq!(
            parse_base_url("http://h:8770/v1/").unwrap(),
            "http://h:8770/v1",
            "a trailing slash is trimmed, as the clients trim it"
        );
        for bad in [
            "",
            "  ",
            "127.0.0.1:8770",
            "ftp://h/v1",
            "http://",
            "http:///v1",
        ] {
            assert!(parse_base_url(bad).is_err(), "{bad}");
        }
        assert!(
            parse_base_url("").unwrap_err().contains("required"),
            "the one question with no default says so"
        );
    }

    /// The wizard asks for the *name* of a variable and never for a token, so a pasted token
    /// has to be refused rather than written into the file `config.example.toml` promises
    /// never holds a secret — and echoed from there into a `check` FAIL row.
    #[test]
    fn a_variable_name_is_accepted_and_anything_shaped_like_a_secret_is_not() {
        // The names people actually use, from three characters to the long end of plausible.
        for good in [
            "MY_KEY",
            "OPENAI_API_KEY",
            "_x1",
            "key",
            "EXAMPLE_API_KEY",
            "  EXAMPLE_API_KEY  ",
            "_private",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "K2",
        ] {
            assert_eq!(parse_env_name(good).unwrap(), good.trim(), "{good}");
        }
        assert_eq!(parse_env_name("").unwrap(), "", "empty = no token at all");

        // Not an identifier at all: every hyphenated or dotted key format.
        for bad in ["sk-live-abc.def", "2MANY", "has space", "a=b"] {
            let e = parse_env_name(bad).unwrap_err();
            assert!(e.contains("not the secret"), "{bad}: {e}");
        }
    }

    /// grok-2: the guard was a length threshold, `len() > 40`, and a classic GitHub PAT is
    /// exactly 40 — a legal identifier with an underscore in it and no bare hex, so every arm
    /// was false. It was accepted, printed back, and written as `api_key_env`.
    ///
    /// Each case below is chosen so that exactly one rule refuses it, and so that the previous
    /// guard would have let it through.
    #[test]
    fn a_credential_pasted_where_the_name_goes_is_refused_by_its_shape() {
        let pat = "ghp_123456789012345678901234567890123456";
        assert_eq!(pat.len(), 40, "the length the old threshold let through");

        let overlong = "A_".repeat(21);
        let cases = [
            (pat, "issuer prefix, and over the length bound"),
            ("ghp_1234567890", "issuer prefix alone: 14 chars, has `_`"),
            ("github_pat_11ABCDEFG0abcdefghijklm", "issuer prefix"),
            ("sk_live_51H8xY2abcdef", "issuer prefix, has `_`"),
            (
                "AKIAIOSFODNN7EXAMPLE",
                "issuer prefix: 20 chars, letters all upper, otherwise a fine name",
            ),
            ("AIzaSyD1234567890abcdefghijklmno", "issuer prefix"),
            ("xoxb1234567890abcdef", "issuer prefix"),
            ("a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4", "bare hex, 32"),
            ("DEADBEEFCAFE12345678", "bare hex, 20, letters all upper"),
            ("aBcDeFgHiJkLmNoPqRsTuVwXyZ01", "28, mixed case, no `_`"),
            ("sk8Kj2LmNpQrStUvWxYz0123456789", "30, mixed case, no `_`"),
            (overlong.as_str(), "42, past the length bound"),
        ];
        for (bad, why) in cases {
            let e = parse_env_name(bad).unwrap_err();
            assert!(
                e.contains("this looks like a secret, not a variable name"),
                "{bad} ({why}): {e}"
            );
            // A rejected answer may well *be* the secret, so no refusal repeats it.
            assert!(!e.contains(bad), "the refusal repeated the answer: {e}");
        }
        assert!(
            !parse_env_name("sk-live-hunter2")
                .unwrap_err()
                .contains("hunter2"),
            "nor does the malformed-identifier refusal"
        );
    }

    #[test]
    fn language_answers_parse_the_way_the_loader_would_read_them() {
        assert_eq!(parse_candidates("").unwrap(), Vec::<String>::new());
        assert_eq!(parse_candidates(" en , ru ").unwrap(), ["en", "ru"]);
        // A trailing comma is a typo, not a language.
        assert_eq!(parse_candidates("en,").unwrap(), ["en"]);
        assert!(parse_candidates("english").is_err());
        assert!(parse_candidates("EN").is_err());

        assert_eq!(parse_by_layout("").unwrap(), Vec::new());
        assert_eq!(
            parse_by_layout(" he=he , ru=en ").unwrap(),
            [
                ("he".to_string(), "he".to_string()),
                ("ru".into(), "en".into())
            ]
        );
        assert!(
            parse_by_layout("he")
                .unwrap_err()
                .contains("layout=language")
        );
        // `auto` is a config-level default, never a per-layout language — `LanguagePolicy`
        // would refuse the file this produced.
        assert!(parse_by_layout("he=auto").is_err());
        assert!(
            parse_by_layout("he=he, he=en")
                .unwrap_err()
                .contains("twice")
        );
    }

    /// A pasted value with an interior control character in it survived every parser, reached
    /// `render`, and killed the TOML parse at the very end — taking nine answered questions
    /// with it, and blaming the wizard for the user's paste. It is a re-ask now.
    #[test]
    fn a_control_character_re_asks_the_question_instead_of_ending_the_run() {
        for e in [
            parse_base_url("http://h:8770/v\u{b}1").unwrap_err(),
            parse_required("dict\u{7}ate").unwrap_err(),
            parse_key_file("~/.config/byovox/e\u{b}nv").unwrap_err(),
        ] {
            assert!(e.contains("control character"), "{e}");
        }
        assert_eq!(parse_key_file("").unwrap(), DEFAULT_KEY_FILE);
        assert_eq!(parse_key_file("  ~/x/env  ").unwrap(), "~/x/env");

        // The run survives it: the next answer is taken and the interview finishes.
        let mut p = Script::new(&[
            "http://h:8770/v\u{b}1", // re-asked
            "http://127.0.0.1:8770/v1",
            "",  // no token
            "n", // no polish
            "",  // hotkey
            "",  // candidates
            "",  // by_layout
            "",  // capture log
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.stt_base_url, "http://127.0.0.1:8770/v1");
        // And what it renders is TOML, which is what used to fail here.
        toml::from_str::<Config>(&render(&a).unwrap()).expect("valid TOML");
    }

    #[test]
    fn yes_no_takes_the_default_on_an_empty_line() {
        assert!(parse_yes_no("", true).unwrap());
        assert!(!parse_yes_no("", false).unwrap());
        assert!(parse_yes_no("Y", false).unwrap());
        assert!(parse_yes_no("yes", false).unwrap());
        assert!(!parse_yes_no("N", true).unwrap());
        assert!(parse_yes_no("maybe", true).is_err());
    }

    /// Scripted answers, and the questions they were answers to.
    struct Script {
        answers: std::collections::VecDeque<String>,
        asked: Vec<String>,
    }

    impl Script {
        fn new(answers: &[&str]) -> Script {
            Script {
                answers: answers.iter().map(|s| s.to_string()).collect(),
                asked: Vec::new(),
            }
        }
    }

    impl Prompter for Script {
        fn ask(&mut self, question: &str) -> Option<String> {
            self.asked.push(question.to_string());
            self.answers.pop_front()
        }
    }

    /// Scripted probe verdicts, plus what each probe was handed.
    struct Verdicts {
        stt: std::collections::VecDeque<bool>,
        polish: std::collections::VecDeque<bool>,
        stt_seen: Vec<Config>,
        polish_seen: Vec<Config>,
    }

    impl Verdicts {
        fn new(stt: &[bool], polish: &[bool]) -> Verdicts {
            Verdicts {
                stt: stt.iter().copied().collect(),
                polish: polish.iter().copied().collect(),
                stt_seen: Vec::new(),
                polish_seen: Vec::new(),
            }
        }
    }

    impl Probe for Verdicts {
        fn stt(&mut self, cfg: &Config) -> bool {
            self.stt_seen.push(cfg.clone());
            self.stt.pop_front().unwrap_or(true)
        }
        fn polish(&mut self, cfg: &Config) -> bool {
            self.polish_seen.push(cfg.clone());
            self.polish.pop_front().unwrap_or(true)
        }
    }

    /// Every answer of a full run, in order: the script below reads as the transcript does.
    const HAPPY: &[&str] = &[
        "http://127.0.0.1:8770/v1", // stt.base_url
        "",                         // stt token variable: none
        "",                         // polish? default yes
        "http://127.0.0.1:4000/v1", // polish.base_url
        "dictate",                  // polish.model
        "",                         // polish token variable: none
        "",                         // hotkey: default ControlRight
        "en, ru",                   // language.candidates
        "he=he",                    // language.by_layout
        "y",                        // capture log
    ];

    /// The walk the wizard exists for: answers in, a file that loads out, with the answers in
    /// it and the documentation still around them.
    #[test]
    fn the_happy_path_writes_every_answer_into_a_file_that_loads() {
        let mut p = Script::new(HAPPY);
        let mut probes = Verdicts::new(&[true], &[true]);
        let a = interview(&mut p, &mut probes).expect("every answer is valid");
        assert!(p.answers.is_empty(), "the script covered every question");

        // The probes saw the answers, not the defaults.
        assert_eq!(probes.stt_seen.len(), 1);
        assert_eq!(probes.stt_seen[0].stt.base_url, "http://127.0.0.1:8770/v1");
        assert_eq!(probes.polish_seen[0].polish.model, "dictate");

        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-happy", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, render(&a).unwrap()).unwrap();

        let cfg = crate::config::load(&path).expect("the wizard's file validates");
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:8770/v1");
        assert_eq!(cfg.stt.api_key_env, "");
        assert!(cfg.polish.enabled);
        assert_eq!(cfg.polish.base_url, "http://127.0.0.1:4000/v1");
        assert_eq!(cfg.polish.model, "dictate");
        assert_eq!(cfg.hotkey.key, "ControlRight");
        assert_eq!(cfg.language.candidates, ["en", "ru"]);
        assert_eq!(
            cfg.language.by_layout.get("he").map(String::as_str),
            Some("he")
        );
        assert!(cfg.capture_log.enabled);
        // Untouched keys keep their documented defaults.
        assert_eq!(cfg.stt.model, "whisper-1");
        assert_eq!(cfg.inject.mode, "auto");

        // And the file is still the documentation.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# byovox configuration."));
        assert!(text.contains("# Sent as the `model` field."));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The probe is handed the config the file will hold, not a second encoding of the same
    /// answers: every key nobody was asked about reaches the endpoint exactly as it will once
    /// the file is written, `[language]` included.
    ///
    /// The one honest gap, asserted here rather than left implied: the language questions come
    /// *after* the STT probe, so `[language]` is still the documented default at that moment
    /// and the probe sends no `language_candidates`. The closing `check` is the run that
    /// exercises the answer — while the user is still sitting there.
    #[test]
    fn a_probe_and_the_written_file_agree_on_every_key_answered_by_then() {
        let mut probes = Verdicts::new(&[true], &[true]);
        let answers = interview(&mut Script::new(HAPPY), &mut probes).unwrap();
        let written: Config = toml::from_str(&render(&answers).unwrap()).unwrap();

        let probed = &probes.stt_seen[0];
        assert_eq!(probed.stt.base_url, written.stt.base_url);
        assert_eq!(probed.stt.model, written.stt.model);
        assert_eq!(probed.stt.timeout_s, written.stt.timeout_s);
        assert_eq!(
            probed.stt.no_speech_threshold,
            written.stt.no_speech_threshold
        );
        assert_eq!(probed.language.default, written.language.default);

        assert!(
            probed.language.candidates.is_empty(),
            "not answered when the STT probe ran"
        );
        assert_eq!(written.language.candidates, ["en", "ru"]);

        // Polish is probed after its own questions, so it agrees on all of them.
        let probed = &probes.polish_seen[0];
        assert_eq!(probed.polish.base_url, written.polish.base_url);
        assert_eq!(probed.polish.model, written.polish.model);
        assert_eq!(probed.polish.min_words, written.polish.min_words);
        assert_eq!(probed.polish.timeout_s, written.polish.timeout_s);
        assert_eq!(probed.polish.prompt_file, written.polish.prompt_file);
    }

    /// Polish declined: no gateway questions are asked and no polish endpoint is written, so
    /// `validate` has nothing to refuse.
    #[test]
    fn declining_polish_asks_no_gateway_questions() {
        let mut p = Script::new(&["http://127.0.0.1:8770/v1", "", "n", "", "", "", ""]);
        let mut probes = Verdicts::new(&[true], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert!(!a.polish_enabled);
        assert!(probes.polish_seen.is_empty(), "no gateway was probed");
        assert!(
            !p.asked.iter().any(|q| q.contains("Chat-completions")),
            "{:?}",
            p.asked
        );
        let cfg: Config = toml::from_str(&render(&a).unwrap()).unwrap();
        assert!(!cfg.polish.enabled);
        assert_eq!(cfg.polish.base_url, "");
    }

    /// A failed probe re-asks the whole group, so the retried answer is the one that gets
    /// probed and the one that gets written.
    #[test]
    fn a_failed_probe_can_be_retried_kept_or_aborted() {
        let mut p = Script::new(&[
            "http://wrong:8770/v1",
            "",
            "r", // retry
            "http://right:8770/v1",
            "",
            "n", // polish off
            "",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[false, true], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert_eq!(a.stt_base_url, "http://right:8770/v1");
        assert_eq!(probes.stt_seen.len(), 2);

        // Keep anyway: a server that is merely not running yet is still the right answer.
        let mut p = Script::new(&["http://later:8770/v1", "", "k", "n", "", "", "", ""]);
        let mut probes = Verdicts::new(&[false], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert_eq!(a.stt_base_url, "http://later:8770/v1");

        // Abort: nothing is returned to write.
        let mut p = Script::new(&["http://nope:8770/v1", "", "a"]);
        let mut probes = Verdicts::new(&[false], &[]);
        let e = interview(&mut p, &mut probes).unwrap_err();
        assert_eq!(e.to_string(), ABORTED);
    }

    /// A bad answer is a re-ask with the reason, never a refusal that loses the run.
    #[test]
    fn an_answer_that_does_not_parse_re_asks_the_same_question() {
        let mut p = Script::new(&[
            "not-a-url",
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "Nope", // not a key name
            "Z",    // a bare letter is not one either
            "Insert",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[true], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert_eq!(a.stt_base_url, "http://127.0.0.1:8770/v1");
        assert_eq!(a.hotkey_key, "Insert");
        let url_qs = p
            .asked
            .iter()
            .filter(|q| q.contains("OpenAI-compatible"))
            .count();
        assert_eq!(url_qs, 2, "asked again after the bad URL");
        let key_qs = p
            .asked
            .iter()
            .filter(|q| q.contains("Push-to-talk"))
            .count();
        assert_eq!(key_qs, 3, "asked again after each bad key name");
        // A chord is accepted where a bare letter is not.
        let mut p = Script::new(&[
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "ControlLeft+ShiftLeft+Z",
            "",
            "",
            "",
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        assert_eq!(a.hotkey_key, "ControlLeft+ShiftLeft+Z");
    }

    /// A variable that is unset here would send the request out with no token — which `check`
    /// refuses outright — so the wizard offers the file byovox reads it from instead.
    #[test]
    fn an_unset_token_variable_offers_the_key_file() {
        let mut p = Script::new(&[
            "http://127.0.0.1:8770/v1",
            "BYOVOX_SETUP_NO_SUCH_TOKEN",
            "", // key file: take the default
            "n",
            "",
            "",
            "",
            "",
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        assert_eq!(a.stt_api_key_env, "BYOVOX_SETUP_NO_SUCH_TOKEN");
        assert_eq!(a.stt_api_key_file, DEFAULT_KEY_FILE);
        assert!(
            p.asked.iter().any(|q| q.contains("key file")),
            "{:?}",
            p.asked
        );

        // A variable that *is* set needs no file, so the question is never asked.
        // SAFETY: no other test reads or writes this name, and the environment API these
        // calls wrap is internally synchronised on Windows.
        unsafe { std::env::set_var("BYOVOX_SETUP_TEST_TOKEN", "not-a-real-token") };
        let mut p = Script::new(&[
            "http://127.0.0.1:8770/v1",
            "BYOVOX_SETUP_TEST_TOKEN",
            "n",
            "",
            "",
            "",
            "",
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        // SAFETY: as above.
        unsafe { std::env::remove_var("BYOVOX_SETUP_TEST_TOKEN") };
        assert_eq!(a.stt_api_key_env, "BYOVOX_SETUP_TEST_TOKEN");
        assert_eq!(a.stt_api_key_file, "", "no file question was asked");
        assert!(
            !p.asked.iter().any(|q| q.contains("key file")),
            "{:?}",
            p.asked
        );
    }

    /// No terminal, no wizard: there is no answer it could invent for `stt.base_url`, so it
    /// says which command does work without one rather than writing a file nobody chose.
    #[test]
    fn a_closed_stdin_aborts_and_names_the_command_that_needs_no_terminal() {
        let e = interview(&mut Script::new(&[]), &mut Verdicts::new(&[], &[])).unwrap_err();
        assert_eq!(e.to_string(), NO_TERMINAL);
        assert!(e.to_string().contains("byovox config --init"));
        // Half an answer is the same fault: the run ends, nothing is written.
        let e = interview(
            &mut Script::new(&["http://127.0.0.1:8770/v1"]),
            &mut Verdicts::new(&[true], &[]),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), NO_TERMINAL);
    }

    /// A lived-in config holds far more than the wizard asks about, so overwriting it is a
    /// decision the user makes at a prompt that names the file.
    #[test]
    fn an_existing_config_is_not_overwritten_without_a_yes() {
        let dir =
            std::env::temp_dir().join(format!("byovox-setup-{}-overwrite", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Nothing there: no question at all.
        let mut p = Script::new(&[]);
        assert!(confirm_overwrite(&mut p, &path).is_ok());
        assert!(p.asked.is_empty());

        std::fs::write(&path, "[stt]\n").unwrap();
        let mut p = Script::new(&[""]);
        let e = confirm_overwrite(&mut p, &path).unwrap_err();
        assert!(e.to_string().contains("left"), "{e}");
        assert!(
            p.asked[0].contains(&path.display().to_string()),
            "{:?}",
            p.asked
        );
        assert!(confirm_overwrite(&mut Script::new(&["n"]), &path).is_err());
        assert!(
            confirm_overwrite(&mut Script::new(&["y"]), &path).unwrap(),
            "a yes is carried to the write, so it is not asked again"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The file the wizard asked about at the start is not the file it writes minutes later.
    /// An interview plus two network round trips is a long window for `config --init` in
    /// another terminal — or a second wizard — to land in, and what lands there is somebody's
    /// config. The write itself is the check: `create_new` cannot be raced.
    #[test]
    fn a_config_that_appears_during_the_interview_is_not_replaced_without_a_yes() {
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-race", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let theirs = "[stt]\nbase_url = \"http://theirs:8770/v1\"\n";

        // Nothing was there when the questions started; something is there now.
        std::fs::write(&path, theirs).unwrap();
        let mut p = Script::new(&[""]); // Enter = no
        let e = write_config(&mut p, &path, "mine = true\n", false).unwrap_err();
        assert!(e.to_string().contains("were not written"), "{e}");
        assert!(
            p.asked[0].contains(&path.display().to_string()),
            "the question names the file: {:?}",
            p.asked
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            theirs,
            "byte for byte what the other writer put there"
        );

        // Only an explicit yes replaces it.
        write_config(&mut Script::new(&["y"]), &path, "mine = true\n", false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine = true\n");

        // And an overwrite granted before the interview is not asked about a second time.
        let mut p = Script::new(&[]);
        write_config(&mut p, &path, "again = true\n", true).unwrap();
        assert!(p.asked.is_empty(), "{:?}", p.asked);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "again = true\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The ordinary path: nothing there, nobody asked, and the config directory is created.
    #[test]
    fn a_fresh_path_is_written_with_its_parent_directory_and_no_question() {
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-fresh", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("byovox").join("config.toml");

        let mut p = Script::new(&[]);
        write_config(&mut p, &path, "fresh = true\n", false).unwrap();
        assert!(p.asked.is_empty(), "{:?}", p.asked);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh = true\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

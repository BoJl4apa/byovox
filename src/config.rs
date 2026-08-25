//! Configuration: one TOML file, every key defaulted, unknown keys rejected.
//!
//! `docs/config.example.toml` is the documentation for every key; a test pins it equal to
//! `Config::default()` so the two cannot drift.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The commented default file shipped in the binary and written by `byovox config --init`.
pub const EXAMPLE: &str = include_str!("../docs/config.example.toml");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub stt: SttConfig,
    pub language: LanguageConfig,
    pub polish: PolishConfig,
    pub hotkey: HotkeyConfig,
    pub inject: InjectConfig,
    pub indicator: IndicatorConfig,
    pub capture_log: CaptureLogConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SttConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key_file: String,
    pub prompt: String,
    pub timeout_s: u64,
    /// Discard a transcript whose `no_speech_prob` exceeds this; `0.0` keeps every one.
    /// TOML floats are f64, and this is the only place the value is stored, printed and
    /// compared against the example file — the pipeline narrows it to f32 at the boundary,
    /// where whisper's own scores live.
    pub no_speech_threshold: f64,
}
impl Default for SttConfig {
    fn default() -> Self {
        Self {
            base_url: "http://your-whisper-host:8770/v1".into(),
            model: "whisper-1".into(),
            api_key_env: String::new(),
            api_key_file: String::new(),
            prompt: String::new(),
            timeout_s: 30,
            no_speech_threshold: 0.3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LanguageConfig {
    /// "auto" or an ISO 639-1 code used when the layout is unmapped or unreadable.
    pub default: String,
    pub candidates: Vec<String>,
    /// keyboard layout language (ISO 639-1) -> explicit STT language.
    pub by_layout: std::collections::BTreeMap<String, String>,
}
impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            default: "auto".into(),
            candidates: Vec::new(),
            by_layout: Default::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolishConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key_file: String,
    pub min_words: usize,
    pub prompt_file: String,
    pub timeout_s: u64,
}
impl Default for PolishConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: "http://your-llm-gateway:4000/v1".into(),
            model: "your-cleanup-alias".into(),
            api_key_env: String::new(),
            api_key_file: String::new(),
            min_words: 0,
            prompt_file: String::new(),
            timeout_s: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyConfig {
    /// W3C UI Events `code` name: ControlRight, AltRight, F13 ... or a chord:
    /// ControlLeft+ShiftLeft+Z.
    pub key: String,
    /// "hold" or "toggle"
    pub mode: String,
    pub min_hold_ms: u64,
    pub cancel_key: String,
}
impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            key: "ControlRight".into(),
            mode: "hold".into(),
            min_hold_ms: 250,
            cancel_key: "Escape".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InjectConfig {
    /// auto | type | paste | clipboard-only
    pub mode: String,
    pub trailing_space: bool,
    /// Longest transcript, in characters, that may be typed; `0` lifts the limit. A reply
    /// over it is held for `byovox last`, never truncated.
    pub max_chars: usize,
}
impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            trailing_space: false,
            max_chars: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndicatorConfig {
    pub pill: bool,
    pub cue: bool,
}
impl Default for IndicatorConfig {
    fn default() -> Self {
        Self {
            pill: true,
            cue: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureLogConfig {
    pub enabled: bool,
    /// empty = `data_dir()` + `capture`, i.e. `%APPDATA%\byovox\data\capture` on Windows,
    /// `~/.local/share/byovox/capture` on Linux.
    pub dir: String,
    /// Delete captures older than this many days; `0` keeps them for ever. The corpus is
    /// voice recordings and verbatim transcripts, so it does not grow unbounded by default.
    pub keep_days: u32,
}
impl Default for CaptureLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: String::new(),
            keep_days: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
}
impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

/// Platform config dir + `config.toml`.
pub fn default_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "byovox")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

/// Platform data dir for capture logs.
pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "byovox")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Bearer token: the named env var, else `NAME=VALUE` in `file` (quotes stripped).
/// Empty `env_name` = no token, so a bare `=VALUE` line can never supply one. A value left
/// blank once quotes and padding come off is `None`, never an empty `Bearer`. Never logged.
pub fn resolve_token(env_name: &str, file: &str) -> Option<String> {
    if env_name.is_empty() {
        return None;
    }
    if let Ok(v) = std::env::var(env_name)
        && !v.trim().is_empty()
    {
        return Some(v.trim().to_string());
    }
    if file.is_empty() {
        return None;
    }
    let path = expand_home(file);
    let text = std::fs::read_to_string(&path).ok()?;
    // The first line naming the key wins; its value is then normalised the same way the
    // env var is, so padding or empty quotes cannot become an empty `Bearer`.
    let raw = text.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        (k.trim() == env_name).then_some(v)
    })?;
    let value = raw.trim().trim_matches('"').trim_matches('\'').trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The one wording for the plain-HTTP warning, so `byovox check` and the daemon's startup
/// log cannot drift into saying different things about the same endpoint.
pub const CLEARTEXT_WARNING: &str = "plain HTTP: voice, transcript and the bearer token cross the network in clear — \
     use https or a private network";

/// Whether this endpoint sends its traffic unencrypted across a network somebody else can be
/// on. True only for `http://` to a non-loopback host.
///
/// Loopback is quiet because it never reaches a wire: a whisper server on the same box is the
/// setup byovox is happiest with. A hostname that is not `localhost` is assumed remote — this
/// runs before any DNS lookup and must not perform one, and a name that happens to resolve to
/// a loopback address is rare enough not to be worth a silent pass.
///
/// Hand-rolled rather than pulling in a URL crate: the question is only "scheme, then host",
/// and `base_url` is a string the user typed, not a URL byovox ever parses for routing.
pub fn is_cleartext_remote(base_url: &str) -> bool {
    let url = base_url.trim();
    let Some(rest) = url
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("http://"))
        .map(|_| &url[7..])
    else {
        return false;
    };
    // Authority only: everything before the path, query or fragment.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        // Credentials in a URL are not something byovox supports, but they must not be
        // mistaken for the host either.
        .rsplit('@')
        .next()
        .unwrap_or_default();
    // `[::1]:8770` keeps its brackets around the address; `host:8770` splits on the colon.
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    // Covers all of 127.0.0.0/8 and ::1 without spelling any of them out.
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => !ip.is_loopback(),
        Err(_) => true,
    }
}

/// `base_url` with any `user:pass@` in its authority replaced by `***@`.
///
/// `byovox check`'s rows are what a user pastes into a bug report, and a `base_url` can carry
/// credentials. Everything else on that surface is already guarded — `check::strip_body`
/// exists only to keep an echoed key out of a row — so an endpoint row must not walk around
/// it. The daemon's own log names keys rather than URLs and needs no equivalent.
///
/// Only the authority is examined: an `@` in a path or query is ordinary and is left alone.
pub fn redact_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://***@{host}{tail}"),
        None => url.to_string(),
    }
}

/// `~/x` → home-relative; anything else unchanged.
pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\"))
        && let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
    {
        return home.join(rest);
    }
    PathBuf::from(p)
}

/// Missing file = all defaults (a fresh install works before `config --init`). Any other
/// read failure is reported, never silently treated as "absent".
pub fn load(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    validate(&cfg).with_context(|| format!("in {}", path.display()))?;
    Ok(cfg)
}

/// The value checks the schema cannot express. Names the offending key, and runs on the one
/// path every command loads through, so a bad value stops the daemon at startup and fails
/// `byovox check` rather than surfacing at the first dictation.
fn validate(cfg: &Config) -> Result<()> {
    let t = cfg.stt.no_speech_threshold;
    // `contains` is false for NaN too, which is the right answer for a threshold nothing
    // could ever compare against.
    if !(0.0..=1.0).contains(&t) {
        bail!("stt.no_speech_threshold is {t}: expected 0.0 to 1.0 (0.0 disables the gate)");
    }
    Ok(())
}

/// Every leaf key in dotted form with "file" if the file sets it, else "default".
pub fn provenance(path: &Path) -> Result<Vec<(String, &'static str)>> {
    let file_table: toml::Table = match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let defaults: toml::Table = toml::Table::try_from(Config::default())?;
    let mut out = Vec::new();
    walk(&defaults, &file_table, "", &mut out);
    Ok(out)
}

fn walk(
    defaults: &toml::Table,
    file: &toml::Table,
    prefix: &str,
    out: &mut Vec<(String, &'static str)>,
) {
    for (k, v) in defaults {
        let dotted = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        // A table with no default entries (`language.by_layout`) is an open-ended map: only
        // the file can name its leaves, so union them in. Nothing from the file = one row
        // for the map itself, so the key is never invisible.
        if matches!(v, toml::Value::Table(t) if t.is_empty()) {
            match file.get(k) {
                Some(toml::Value::Table(ft)) if !ft.is_empty() => {
                    out.extend(ft.keys().map(|fk| (format!("{dotted}.{fk}"), "file")));
                }
                _ => out.push((dotted, "default")),
            }
            continue;
        }
        match (v, file.get(k)) {
            (toml::Value::Table(dt), Some(toml::Value::Table(ft))) => walk(dt, ft, &dotted, out),
            (toml::Value::Table(dt), _) => walk(dt, &toml::Table::new(), &dotted, out),
            (_, Some(_)) => out.push((dotted, "file")),
            (_, None) => out.push((dotted, "default")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_defaults_for_everything_else() {
        let cfg: Config = toml::from_str("[stt]\nbase_url = \"http://x:1/v1\"\n").unwrap();
        assert_eq!(cfg.stt.base_url, "http://x:1/v1");
        assert_eq!(cfg.stt.timeout_s, 30);
        assert_eq!(cfg.hotkey.key, "ControlRight");
        assert!(cfg.polish.enabled);
    }

    #[test]
    fn unknown_key_is_an_error() {
        let err = toml::from_str::<Config>("[stt]\nbase_ur = \"typo\"\n").unwrap_err();
        assert!(err.to_string().contains("base_ur"), "{err}");
    }

    #[test]
    fn example_file_is_exactly_the_defaults() {
        let from_example: Config = toml::from_str(EXAMPLE).expect("example parses");
        assert_eq!(from_example, Config::default());
        // Table-level too: a key added to the schema but left out of the example file
        // deserialises to its default and would slip past the struct comparison above.
        assert_eq!(
            toml::from_str::<toml::Table>(EXAMPLE).unwrap(),
            toml::Table::try_from(Config::default()).unwrap()
        );
    }

    #[test]
    fn provenance_marks_file_keys() {
        let dir = tempfile_dir("marks_file_keys");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[polish]\nmodel = \"x\"\n").unwrap();
        let prov = provenance(&path).unwrap();
        assert!(prov.contains(&("polish.model".to_string(), "file")));
        assert!(prov.contains(&("polish.enabled".to_string(), "default")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn provenance_lists_file_supplied_map_entries() {
        let dir = tempfile_dir("map_entries");

        let with = dir.join("with.toml");
        std::fs::write(&with, "[language.by_layout]\nhe = \"he\"\n").unwrap();
        let prov = provenance(&with).unwrap();
        assert!(
            prov.contains(&("language.by_layout.he".to_string(), "file")),
            "{prov:?}"
        );

        let without = dir.join("without.toml");
        std::fs::write(&without, "[polish]\nmodel = \"x\"\n").unwrap();
        let prov = provenance(&without).unwrap();
        assert!(
            prov.contains(&("language.by_layout".to_string(), "default")),
            "{prov:?}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_loads_the_defaults() {
        let dir = tempfile_dir("missing_file");
        let path = dir.join("config.toml");
        assert_eq!(load(&path).unwrap(), Config::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A probability outside 0..1 gates nothing or gates everything, and either way the file
    /// says something the pipeline cannot honour. It has to name the key: the daemon loads
    /// this file long before any dictation could show the value was wrong.
    #[test]
    fn a_threshold_outside_the_unit_range_names_the_key() {
        let dir = tempfile_dir("threshold_range");
        let path = dir.join("config.toml");
        for bad in ["1.5", "-0.1", "nan"] {
            std::fs::write(&path, format!("[stt]\nno_speech_threshold = {bad}\n")).unwrap();
            // `{:#}` is how `main` prints a fatal, so this is the text the user sees.
            let msg = format!("{:#}", load(&path).unwrap_err());
            assert!(msg.contains("stt.no_speech_threshold"), "{bad}: {msg}");
            assert!(msg.contains(&path.display().to_string()), "{bad}: {msg}");
        }
        // The ends are usable: 1.0 gates only a certainty, 0.0 turns the gate off.
        for good in ["0.0", "1.0", "0.6"] {
            std::fs::write(&path, format!("[stt]\nno_speech_threshold = {good}\n")).unwrap();
            assert!(load(&path).is_ok(), "{good}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unparsable_file_names_the_path() {
        let dir = tempfile_dir("unparsable");
        let path = dir.join("config.toml");
        std::fs::write(&path, "not = [toml").unwrap();
        let err = load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_token_prefers_env_then_file() {
        let dir = tempfile_dir("resolve_token");
        let f = dir.join("env");
        std::fs::write(&f, "OTHER=1\n=leaked\nMY_TOKEN=\"from-file\"\n").unwrap();
        // SAFETY: no other test reads or writes `MY_TOKEN`, and the environment API these
        // calls wrap is internally synchronised on Windows.
        unsafe { std::env::remove_var("MY_TOKEN") };
        assert_eq!(
            resolve_token("MY_TOKEN", f.to_str().unwrap()),
            Some("from-file".into())
        );
        // An empty name matches nothing, so the bare `=leaked` line cannot hand back a
        // token the user never named.
        assert_eq!(resolve_token("", f.to_str().unwrap()), None);
        // SAFETY: as above.
        unsafe { std::env::set_var("MY_TOKEN", "from-env") };
        assert_eq!(
            resolve_token("MY_TOKEN", f.to_str().unwrap()),
            Some("from-env".into())
        );
        assert_eq!(resolve_token("", ""), None);
        assert_eq!(resolve_token("NOPE_UNSET", ""), None);
        // SAFETY: as above.
        unsafe { std::env::remove_var("MY_TOKEN") };
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Both stages read their token the same way. STT was env-var-only, which left its token
    /// nowhere but the environment — readable by anything running as the user and inherited
    /// by every process byovox spawns — while polish could keep one in a file.
    #[test]
    fn both_stages_resolve_a_token_from_a_key_file() {
        let dir = tempfile_dir("stt_key_file");
        let f = dir.join("env");
        std::fs::write(&f, "STT_TOKEN=\"from-file\"\nPOLISH_TOKEN=other\n").unwrap();
        // SAFETY: no other test reads or writes these names, and the environment API these
        // calls wrap is internally synchronised on Windows.
        unsafe { std::env::remove_var("STT_TOKEN") };

        let cfg = SttConfig {
            api_key_env: "STT_TOKEN".into(),
            api_key_file: f.to_str().unwrap().into(),
            ..Default::default()
        };
        assert_eq!(
            resolve_token(&cfg.api_key_env, &cfg.api_key_file),
            Some("from-file".into())
        );
        // The default is still no token at all: naming no variable means an endpoint that
        // authenticates some other way, and the file is never even opened.
        let bare = SttConfig::default();
        assert_eq!(bare.api_key_file, "");
        assert_eq!(resolve_token(&bare.api_key_env, &bare.api_key_file), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_token_normalises_the_file_value() {
        let dir = tempfile_dir("resolve_token_value");

        let padded = dir.join("padded");
        std::fs::write(&padded, "PADDED_TOKEN=\" from-file \"\n").unwrap();
        assert_eq!(
            resolve_token("PADDED_TOKEN", padded.to_str().unwrap()),
            Some("from-file".into())
        );

        // Empty quotes must not become an empty `Bearer` header.
        let blank = dir.join("blank");
        std::fs::write(&blank, "BLANK_TOKEN=\"\"\n").unwrap();
        assert_eq!(resolve_token("BLANK_TOKEN", blank.to_str().unwrap()), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Plain HTTP to somewhere off this machine puts the audio, the transcript and the
    /// bearer token on the wire in clear, and lets anyone on the path replace the reply with
    /// text byovox will type. Loopback never reaches a wire and must stay quiet, or the
    /// warning becomes noise for the setup byovox recommends.
    #[test]
    fn only_plain_http_to_somewhere_off_this_machine_warns() {
        for quiet in [
            "http://127.0.0.1",
            "http://127.0.0.1:8770/v1",
            "http://127.5.6.7:8770/v1",
            "http://localhost",
            "http://localhost:8770/v1",
            "http://LocalHost:8770/v1",
            "http://[::1]",
            "http://[::1]:8770/v1",
            "https://api.example.com/v1",
            "https://10.0.0.5:8770/v1",
            "HTTPS://api.example.com/v1",
            // Not an absolute HTTP URL at all: `stt.base_url` is validated by the request
            // failing, and this predicate must not invent a warning about it.
            "",
            "your-whisper-host:8770/v1",
        ] {
            assert!(!is_cleartext_remote(quiet), "{quiet} must not warn");
        }
        for warns in [
            "http://10.0.0.5",
            "http://10.0.0.5:8770/v1",
            // 100.64.0.0/10 is the CGNAT range Tailscale hands out: still a network, still
            // in clear, so it warns like any other non-loopback address.
            "http://100.64.0.1:4000/v1",
            "http://your-whisper-host:8770/v1",
            "http://[2001:db8::1]:8770/v1",
            "HTTP://10.0.0.5:8770/v1",
            "  http://10.0.0.5:8770/v1  ",
            // Credentials must not be read as the host: the host here is example.com.
            "http://user:pass@example.com/v1",
        ] {
            assert!(is_cleartext_remote(warns), "{warns} must warn");
        }
    }

    /// `check`'s rows get pasted into bug reports, so a `base_url` carrying credentials must
    /// not print them. Only the authority is touched — an `@` in a path is ordinary.
    #[test]
    fn credentials_never_survive_into_a_printed_url() {
        assert_eq!(
            redact_userinfo("http://user:pass@example.com/v1"),
            "http://***@example.com/v1"
        );
        assert_eq!(
            redact_userinfo("https://token@10.0.0.5:8770/v1?x=1"),
            "https://***@10.0.0.5:8770/v1?x=1"
        );
        // An `@` inside the userinfo itself: the last one separates, so the whole of it goes.
        assert_eq!(
            redact_userinfo("http://us@er:p@ss@example.com/v1"),
            "http://***@example.com/v1"
        );
        for untouched in [
            "http://example.com/v1",
            "http://10.0.0.5:8770/v1",
            "http://[::1]:8770/v1",
            // `@` in the path and the query is not userinfo.
            "http://example.com/a@b",
            "http://example.com/v1?to=a@b",
            "",
            "not-a-url",
        ] {
            assert_eq!(redact_userinfo(untouched), untouched, "{untouched}");
        }
        // The redaction leaves nothing of the secret behind.
        assert!(!redact_userinfo("http://u:hunter2@example.com/v1").contains("hunter2"));
    }

    /// A directory unique to this process *and* this test, so tests running in parallel
    /// never share a file. Each caller removes it once it is done.
    fn tempfile_dir(test: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("byovox-test-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}

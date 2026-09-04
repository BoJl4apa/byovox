# byovox — design

**Status:** living architecture document. Originally the approved design of 2026-08-24; kept
true to the code since, and re-verified against it by the docs drift audit. Read it as a
description of what byovox *is*, not of what was once intended — where a section describes work
that has not been built, it says so under its own heading.

**Implemented today: Windows only.** Linux and macOS are designed but not written; that design
is preserved in [Planned, not built](#planned-not-built) and no backend for either platform
exists in the tree.

byovox ("bring your own vox") is a push-to-talk dictation client for the desktop. Hold a
key, speak, release: the audio goes to a speech-to-text endpoint you run, the transcript
optionally goes through a cleanup model you run, and the result is typed into whatever
has focus. It is one Rust crate over two binaries — a console CLI and a windowless daemon —
with no UI beyond a tray icon and a recording indicator, configured by one TOML file.

It exists because every off-the-shelf client treats STT as a fixed vendor and makes only
the cleanup model pluggable. When you own the STT server, the client needs to speak to it
fully — `language`, `prompt`, and language candidates per request — and it needs to know
which language you are about to speak. No existing client does the second thing at all.

## Goals

- **Layout-routed language.** Read the foreground window's keyboard layout at the moment
  the hotkey is pressed and map it to the STT request: an explicit `language` for layouts
  you configure, auto-detection restricted to a candidate list for the rest.
- **Endpoint-faithful.** Any OpenAI-compatible `/v1/audio/transcriptions` and
  `/v1/chat/completions` work. `language` and `prompt` are standard fields.
  `language_candidates` is a whisper.cpp-server extension carried by the maintainer's fork
  (which is what the reference deployment runs) — servers that lack it ignore the field and fall back to unconstrained
  auto-detection, which is what they would have done anyway.
- **Never lossy.** A dictation that reached the STT server is inserted even if every later
  stage fails. Three outcomes are not an insert, and each is deliberate:
  - a transcript whisper itself scored as silence above `stt.no_speech_threshold` is **dropped**
    — decided on that score and never on the words, so no utterance is ever discarded for what
    it happens to say, and the clip is kept in the capture log. `stt.no_speech_threshold = 0`
    removes it.
  - a reply that is **empty by the time it would be typed** — nothing but forbidden characters,
    or a lone period the terminal-period strip removes — ends as an empty dictation. It is
    deliberately not held for `byovox last`: that command must never hand back something the
    user did not dictate.
  - a transcript longer than `inject.max_chars` (default 20 000) is **not typed and not cut
    down**: truncating would insert half a dictation and silently discard the rest. It is held
    whole for `byovox last`, which is the one place the pipeline promises never to lose text.
- **Multiplatform with honest degradation.** The aim is Windows and Linux on KDE Plasma
  (Wayland), with GNOME, X11 and macOS written to the same seams. **Only Windows is built
  today.** The property that matters is the one already in force: every platform capability
  degrades one rung at a time and reports which rung it is on, so a missing backend costs a
  feature and never a dictation.
- **Documented config.** The config schema *is* the documentation. `docs/config.example.toml`
  is the one commented default: compiled into the binary for `byovox config --init`, pinned
  by a test against `Config::default()`, and *linked* from the README rather than copied into
  it by a build script — one canonical text beats two that can drift. The effective-config
  printout comes off the same struct.
- **Lean.** Tray + indicator only. No settings window, no auto-update, no account.

## Non-goals

- Mobile. Android/iOS are separate stacks; they stay on config-only clients pointed at the
  same endpoints.
- Streaming transcription, local/offline models, wake words, always-on listening. The
  microphone is open only between press and release.
- A settings UI. The TOML file and `byovox config` are the UI.

## Architecture

One crate, two binaries: the console CLI with its subcommands, and the windowless daemon.

| Command | Purpose |
|---|---|
| `byovox` | Start the daemon in the background. Single instance. |
| `byovox run` | Run the daemon in this console instead — tray icon, hotkey listener, pipeline, and the log on stderr as well as in the file. |
| `byovox toggle` | Signal the running daemon to start/stop recording. The path for OS-bound shortcuts and for setups without an in-process hotkey. |
| `byovox quit` | Stop the daemon. |
| `byovox last` | Print the most recent transcript held by the daemon (memory only, cleared on quit) — the retrieval path when no inject rung worked. |
| `byovox status` | Print the daemon's state and its last error. |
| `byovox setup` | Interactive wizard: ask for each stage's endpoint, probe every answer, write the config file. |
| `byovox check` | Self-test every stage and print the rung chosen per backend. |
| `byovox hotkey [--set\|--list\|--mode\|--cancel\|--force]` | Show the push-to-talk binding, list the key names accepted, or change it in place with the comments intact. |
| `byovox config [--init]` | Print the effective config with value provenance, or write a fully commented default file. |
| `byovox autostart --enable\|--disable` | Register/unregister with the OS's per-user autostart. |

### The pipeline is a state machine

```
Idle ──press──▶ Recording ──release──▶ Transcribing ──▶ Polishing ──▶ Inserting ──▶ Idle
                    │ held < min_hold_ms → discard (no request)
                    │ cancel key → discard
Transcribing error → Idle, nothing inserted, error indication, logged
Polishing error    → Inserting with the RAW transcript, error indication, logged
Inserting error    → next inject rung (…→ clipboard-only → none), error indication, logged
```

Once a transcript exists, every later failure still ends in an insert attempt — that is
the never-lossy guarantee, and the unit tests pin it.

In `toggle` mode a press toggles between Idle and Recording; releases are ignored. A press
while Transcribing/Polishing/Inserting is ignored.

### Five traits carry every platform difference

```rust
trait Hotkey   { fn run(self: Box<Self>, tx: Sender<HotkeyEvent>) }  // Pressed | Released | Toggle | Cancel
trait Capture  { fn start(&mut self) -> Result<()>; fn stop(&mut self) -> Result<Audio> }   // 16 kHz mono i16
trait Layout   { fn current(&self) -> Option<Lang> }             // ISO 639-1 of the foreground window's layout
trait Inject   { fn name(&self) -> &'static str; fn inject(&mut self, text: &str) -> Result<(), String> }
trait Indicator{ fn set(&mut self, state: IndicatorState) }      // Idle | Recording | Working | Done | Uncertain (#26) | Error
```

`Inject::name` is not incidental: it is what the pipeline records as the rung used, what
`byovox check` prints, and what the capture-log row carries.

Five more traits are seams rather than platform differences — `Transcriber`, `Polisher` and
`Recorder` let the pipeline's tests run without a network or a microphone, and `Prompter` and
`Probe` let `byovox setup`'s whole interview be scripted in a unit test.

The pipeline holds boxed trait objects and knows no OS. `platform::detect()` probes the
box at startup, picks an implementation per trait, and logs the choice; `byovox check`
prints it. Runtime selection is why one binary could serve KDE-with-portals and
GNOME-with-evdev alike — the reason the seams are traits rather than `#[cfg]` branches, even
though only one platform is implemented so far.

### Threads

- **main** — the `winit` event loop, and with it the tray icon and menu, the floating pill and
  the audio cues. Everything that must be on the main thread lives in `ui.rs`; the pipeline
  reaches it only by posting `UserEvent`s through a `ProxyIndicator`.
- **`byovox-pipeline`** — blocks on the event channel; runs capture control, HTTP calls
  (synchronous, `ureq`), injection. Strictly sequential; no async runtime. It is wrapped in
  `catch_unwind`: a panic there leaves through the ordinary Quit path, because a tray icon
  that still says idle is worse than no tray icon.
- **`byovox-hotkey`** — the Win32 low-level hook needs its own message pump; it pushes events
  into the channel and does nothing else.
- **`byovox-ipc`** — accepts on the per-user socket and answers one connection per thread.
- **audio callback** — `cpal`'s own thread appends samples to a buffer while Recording.

### Modules

```
src/main.rs         CLI (clap) and subcommand dispatch; the daemon itself is byovox::daemon
src/bin/byovox-daemon.rs   the windowless binary (Windows subsystem)
src/daemon.rs       everything that runs while the tray icon is up: wiring, threads, logging
src/config.rs       schema (serde), defaults, provenance; EXAMPLE is docs/config.example.toml
src/pipeline.rs     the state machine, and `sanitize` — the last point the reply is just data
src/stt.rs          transcription client (multipart, policy fields, per-language lane routing)
src/polish.rs       cleanup client, the built-in prompt, and the glossary rule composed onto it
src/lang.rs         language policy: keyboard layout in, STT request fields out
src/audio.rs        16 kHz mono i16 buffer, the WASAPI start-click cut, WAV encoding
src/capture.rs      cpal input; opened on press, closed on release, never held open
src/hotkey.rs  src/layout.rs  src/inject.rs  src/indicator.rs   traits, chord grammar, rungs
src/ui.rs           the winit event loop, tray, pill and cues — all main-thread
src/ipc.rs          per-user socket: toggle / quit / status / last, one JSON object per line
src/capture_log.rs  opt-in per-dictation dump, and the pruner
src/check.rs        `byovox check`
src/setup.rs        `byovox setup`   src/hotkey_cmd.rs  `byovox hotkey`
src/ollama.rs       Ollama discovery, asked of the daemon on this machine
src/multipart.rs    the ~30 lines that let us stay on `ureq`
src/testutil.rs     a one-thread HTTP double that records the raw request
src/platform/mod.rs detect() and the rung order
src/platform/windows/…   hotkey, layout, inject, audio, autostart
```

Single crate, two binaries. No workspace until a third needs one.

## Backends

Windows is the only platform with backends in the tree. `platform::detect()` returns them, or
refuses outright on anything else: *"no backends for this platform yet — Linux (KDE) lands in
the next plan, macOS after"*.

| Trait | Windows implementation |
|---|---|
| Hotkey | `WH_KEYBOARD_LL` hook on a message-pump thread; press/release for any key, including a bare modifier. Chords are held: all members down, and letting go of any one ends the recording. Only a chord's trigger is swallowed. |
| Capture | `cpal`/WASAPI, opened on press and closed on release. `capture.device` pins an input by name; the first samples are cut when they are WASAPI's start click (#32). |
| Layout | `GetForegroundWindow` → `GetWindowThreadProcessId` → `GetKeyboardLayout` → LANGID, normalised to ISO 639-1. |
| Inject | `SendInput` + `KEYEVENTF_UNICODE` (`type`), clipboard + Ctrl+V with the previous **text** restored ~150 ms later (`paste`), and `clipboard-only`. |
| Indicator | tray (`tray-icon`), pill (`winit` + `softbuffer`), cues (`rodio`, synthesised at run time). |

**Rungs.** `inject.mode = "auto"` (the default) tries `type` → `paste` → `clipboard-only` in
that order, falling to the next when one fails; naming a mode pins that single rung. A rung the
platform cannot provide is a startup error, never a silent substitution. Where every rung fails
the transcript is held in memory for `byovox last` and the tray's *Show last transcript*, the
error cue plays, and nothing about the text is logged or notified.

**Universal fallbacks.** Hotkey → `byovox toggle` bound to an OS shortcut. Layout → `None` →
the default policy. A missing backend never fails a dictation; it degrades one rung and logs
which.

## Pipeline detail

1. **Press.** Read the layout immediately (focus may change later), open the microphone,
   indicator → Recording, start cue.
2. **Release.** Close the microphone. Held under `hotkey.min_hold_ms` (default 250) →
   discard silently. `hotkey.cancel_key` (default Escape) during Recording → discard.
3. **Encode.** Downmix to mono by averaging, resample to 16 kHz with a windowed-sinc
   resampler (`rubato`), encode 16-bit PCM WAV in memory (`hound`).
4. **Transcribe.** Multipart POST to `{stt.base_url}/audio/transcriptions` — or to
   `{stt.by_language.<code>.base_url}` when the policy resolved to a language with a lane
   configured, falling back to the default endpoint if the lane answers with nothing (#14):
   `file`,
   `response_format` (`verbose_json` while `stt.no_speech_threshold > 0`, since that is the
   only format carrying `segments[].no_speech_prob`; plain `json` when the gate is off, which
   is the wire a server that refuses `verbose_json` needs), `model` (sent, ignored by most
   self-hosted servers), the language policy fields (below), and `Authorization: Bearer` when
   a token resolves — from `stt.api_key_env`, or from a `NAME=VALUE` line in `stt.api_key_file`
   when that variable is unset here. Polish takes the same pair. Timeouts: connect 5 s, total `stt.timeout_s` (30).
   One retry on connection error only; HTTP errors are never retried — they are
   configuration problems and retrying hides them. Response `text` is trimmed; the strongest
   `no_speech_prob` over the reply's segments is read alongside it, and is absent when the
   server sends no segments.
5. **Nothing to insert** → Idle, logged at INFO, no cue. Ways in: an empty transcript —
   including one emptied later by step 7's sanitising (#11) or period strip (#19) —
   and a transcript scored above `stt.no_speech_threshold` (default 0.3), which whisper fills
   with an invented stock phrase over near-silence. Neither polishes nor injects nor is held
   for `byovox last`; the score decides and the text is never inspected. The INFO line for
   the second carries the probability and no text. Measured bands: speech ≤ 0.08, silent
   holds 0.54–0.77.
6. **Polish** when `polish.enabled` and word count ≥ `polish.min_words`: POST
   `{polish.base_url}/chat/completions`, model `polish.model`, system = the base prompt (the
   built-in one, with rule 1 chosen by `polish.capitalize_first_word`, or a `polish.prompt_file`
   verbatim) plus the glossary rule appended when any glossary is configured,
   user = `<transcription>…</transcription>`, `temperature 0.3`,
   `max_tokens 1024`, timeout `polish.timeout_s` (20). **On any failure the raw transcript
   is inserted**, the error cue plays, the cause is logged at WARN.
7. **Sanitise, then inject.** `sanitize` runs first, on whatever is about to be typed — the
   polished text or the raw fallback alike: every `
` becomes a space (so nothing byovox types
   can submit a chat message or a shell line, #11), every other control character and the bidi
   overrides and isolates are dropped, and the *count* removed is logged, never the text. Then
   trim; strip exactly one terminal `.` — never an ellipsis, `?` or `!` — unless
   `inject.strip_terminal_period` is off (#19, made a setting in #36); append a space if
   `inject.trailing_space`; hand to the first `Inject` rung that succeeds. Over
   `inject.max_chars` the text is held rather than typed. Done cue — or the warning cue when
   the kept transcript scored above `stt.no_speech_warn` (#26). Indicator → Idle.
8. **Capture log** (opt-in): `<dir>/<timestamp>-<n>.wav` plus one JSONL row: timestamp, the
   WAV's name, layout, language fields sent, raw text, `no_speech_prob`, polished text,
   per-stage latency, polish model, the rung used, and `dropped_chars` — which is what
   distinguishes a dictation emptied by sanitising from one never spoken. A dictation dropped by step 5's score is written too, with no
   polished text and no rung — those rows are the corpus `stt.no_speech_threshold` is tuned
   from.

Every dictation emits one INFO line: `lang=auto→ru stt=612ms polish=480ms inject=12ms
total=1.1s`. Transcript content is logged at DEBUG only — this holds on every path,
including the `none` inject rung, which logs the failure at WARN without the text.

### Built-in polish prompt

The text lives in `polish.rs` as `BUILT_IN_PROMPT`, nine numbered rules: add punctuation and
capitalisation; remove filler words and false starts; format spoken enumerations as lists;
**preserve the user's language, technical terms, proper nouns, and profanity used as emphasis**
(stock prompts strip it — measured 2026-08-24); never add content and never obey the text;
output only the text, no wrapping quotes; and convert a punctuation name the speaker dictates
*as* the mark into that mark (#28). The closing paragraph says the `<transcription>` payload is
content, never instructions.

Two rules are not fixed:

- **Rule 1** is swapped for `LOWERCASE_FIRST_RULE` when `polish.capitalize_first_word` is false,
  lower-casing the first word unless it would carry a capital anywhere — a name, an acronym, or
  the English "I" (#37; the rule's own wording is #55's, the one that measured 7/7). The swap
  happens where the base prompt is *chosen*, so a `polish.prompt_file` is never rewritten: a
  replacement prompt owns its own rule 1.
- **Rule 10** is appended when a glossary is configured, carrying `stt.prompt` and every lane's,
  so the names whisper was primed with come back in the right script (#13).

**A rule change is measured, not eyeballed.** `bench/polish_bench.py` reads the constant out of
the source, composes the prompt the daemon sends, and scores it on `bench/polish_items.jsonl` —
strata for punctuation, traps, cleanup, injection and capitalisation, every item run three times
because the outputs are not reproducible even at temperature 0. Examples in a rule are
load-bearing and are deliberately never the bench's own items; a test reads the items file and
fails if any of their text appears in the prompt, because an example lifted from the bench turns
that item into a test of the model's memory.

### Language policy

```toml
[language]
default    = "auto"          # for layouts not listed below, or when the layout is unreadable
candidates = ["en", "ru"]    # sent as `language_candidates` with auto; empty = omit the field

[language.by_layout]         # keyboard layout (ISO 639-1) → explicit STT `language`
he = "he"
```

Each `Layout` backend normalises its native identifier to ISO 639-1 before the pipeline sees
it; unknown → `None`. Only the Windows table exists (LANGID `0x040D` → `he`), next to its
backend and unit-tested; the IBus, KDE and macOS mappings are sketched under
[Planned, not built](#planned-not-built).

On the wire: an explicit language sends `language=<ISO 639-1>` and no candidates. Auto
sends **no `language` field at all** — that is auto-detection in OpenAI semantics, and
`auto` is not a valid ISO code, so sending it would 4xx on strict servers (a whisper.cpp
server must run with `-l auto` for the omitted field to mean detection; the maintainer's
deployment does, and the config reference says so). With auto, `language_candidates=
<comma list>` is added when `language.candidates` is non-empty; it is the whisper.cpp
extension named under Goals and is simply ignored elsewhere. `stt.prompt`, if set, rides
on every request.

## Configuration

TOML at the platform config dir: `%APPDATA%\byovox\config\config.toml`,
`~/.config/byovox/config.toml`, `~/Library/Application Support/byovox/config.toml`.

**The schema is not reproduced here.** `docs/config.example.toml` is the one commented text —
compiled into the binary with `include_str!`, written by `byovox config --init`, and pinned by
a test (`example_file_is_exactly_the_defaults`) that parses it and compares the result to
`Config::default()` key by key. A second copy in this document could only drift from it, and
did: it was missing ten shipped keys when the drift audit read it.

The sections, and what each is for:

| Section | For |
|---|---|
| `[stt]` | the speech endpoint, its token pair, the vocabulary `prompt`, the two no-speech thresholds, and `hosted` |
| `[stt.by_language.<code>]` | a per-language lane: its own `base_url`, and `model`/`prompt` where they differ (#14) |
| `[language]` | `default`, `candidates`, and `by_layout` — the policy in §Language policy |
| `[polish]` | the cleanup endpoint and token pair, `min_words`, `prompt_file`, `capitalize_first_word`, `hosted` |
| `[hotkey]` | `key` (a name or a chord), `mode` hold/toggle, `min_hold_ms`, `cancel_key` |
| `[inject]` | `mode`, `trailing_space`, `strip_terminal_period`, `max_chars` |
| `[indicator]` | `pill`, `cue` |
| `[capture]` | `device` — pin an input by name |
| `[capture_log]` | `enabled`, `dir`, `keep_days` |
| `[logging]` | `level` |

Every key has a default and unknown keys are refused, so a typo is a startup error naming the
key rather than a setting that silently does nothing. Three keys have no default byovox could
guess and are required before anything runs: `stt.base_url`, and `polish.base_url` and
`polish.model` while polish is enabled. `hosted` changes no request — it is the record of a
choice, so `byovox check` can say a stage is somebody else's machine when no URL could reveal
it (#41).

Nothing is generated at build time — `build.rs` does one thing, stamp the commit into
`BYOVOX_GIT_SHA`. The README *links* the reference rather than reproducing it, one canonical
text beating two that can drift. `byovox config` prints the effective value of every key
tagged `file` or `default`, which says where that value came from.

## Operations

**Single instance.** A named pipe (`\\.\pipe\byovox`) or a Unix socket in
`$XDG_RUNTIME_DIR`. Protocol: one JSON object per line each way. Request
`{"cmd": "toggle" | "quit" | "status" | "last"}`; reply `{"ok": true, ...}` or
`{"ok": false, "error": "<message>"}`. `status` replies with the pipeline state and the
last error; `last` replies `{"ok": true, "text": "<transcript>"}` — JSON escaping
carries newlines, so a polished list arrives intact (superseded by #9: a held transcript
never contains a newline; they are flattened to spaces before anything is typed or held) —
or `ok: false` when nothing is held. Anything unparseable gets `ok: false` and the connection closes.

**Errors — loud, never silent, never lossy:**

| Where | Behaviour |
|---|---|
| Config invalid / unknown key | refuse to start, exit 2, name the key |
| Backend cannot initialise | degrade one rung, WARN with the rung chosen; `check` shows it |
| STT fails | error cue, tray tooltip carries the last error, ERROR log; nothing inserted |
| Polish fails | raw transcript inserted, error cue, WARN |
| Inject fails | next inject rung, down to `none`: transcript held for `byovox last`, error cue, notification without the text, WARN (event only, no content) |

**Logging.** `tracing` to a rotated file under the platform local-data dir
(`%LOCALAPPDATA%\byovox\data\logs`, `~/.local/share/byovox/logs`,
`~/Library/Application Support/byovox/logs`), plus stderr under `byovox run` — the daemon
binary has no console to write to. Levels per the usual contract: ERROR operation failed, WARN unexpected
but continuing, INFO milestones (one line per dictation), DEBUG detail (the only level
that carries transcript text).

**`byovox check`.** Config validity → chosen rung per backend → open the microphone for
one second and print the peak level → read the layout and print the policy it resolves
to → post that second of audio to STT with latency → send a fixed sample dictation through the
exact prompt the daemon would compose, with latency → inject dry-run. It also prints a
`warn network` row per cleartext endpoint and a `note hosted` row per stage marked hosted.
Non-zero exit if a required stage fails; warn and note rows never change the exit code, because
a script gating on it needs it to keep meaning "byovox can dictate".

**Tray menu.** Status line (last error if any) · Enable/Disable · Mode hold/toggle · Show
last transcript · Open config · Open logs · Run check · Quit. Icons: idle, recording,
working, error (3 s).

**Indicator.** Pill: a small frameless always-on-top window near the cursor showing
"● recording" / "… working", hidden when idle. Cues: four
short tones (start, done, warning, error), synthesised at run time rather than shipped as
assets — nothing to load, nothing to fail to load. Only a dictation that reached the
focused window plays the done tone: a tap, a cancel and an empty transcript are silent.
The cues follow the default output device rather than staying on the one they opened on,
and the tray carries an "Audio cues" item that silences them for the running daemon
without writing `indicator.cue` (see `docs/platform-windows.md`, which is current).

**Autostart.** One value under HKCU `Run`, holding an absolute path to `byovox-daemon` plus
any `--config` given, absolutised. (XDG autostart `.desktop` and a LaunchAgent plist are the
planned equivalents.) **Updates:** none built in; GitHub release binaries and `cargo install`.

## Testing

**Unit (no hardware, CI on all three OSes)**, using fake backends the traits make
natural: config defaults and unknown-key rejection; every layout normalisation table;
policy resolution to exact request fields; downmix/resample/WAV byte-exact against
fixtures; multipart body against a fixture; state machine: tap-discard, cancel, ignore-
while-busy, every error path — polish failure must provably insert the raw text; IPC
protocol parsing; capture-log row shape.

**Integration (opt-in, needs endpoints):** `byovox check`, and `bench/polish_bench.py` — the
polish stage is text to text, so scoring a prompt change needs no audio, only the endpoint.

**Gates on every push** (`.github/workflows/ci.yml`): `cargo fmt --check`, `cargo clippy
--all-targets --locked -D warnings`, `cargo test --locked` and `bench/polish_bench.py
--self-test` on a windows/ubuntu/macos matrix, a `cargo check` against the declared MSRV so
`rust-version` is a checked promise, and `cargo audit` + `cargo deny check`. All on the locked
dependency versions.

The bench self-test is there because the couplings it checks are the ones a Rust-only suite
cannot see: the bench reads `BUILT_IN_PROMPT` and the two rule-1 constants *out of the source*,
so a reshaped literal or a renumbered rule desyncs it silently. When #28 renumbered the
glossary rule across four sites, `cargo test` covered three of them and the bench's own
extraction regex was the fourth.

**Manual checklist** (`docs/testing.md`), run against the release build before a tag:
hold/release/tap/cancel; layout switch → language; injection into an editor, a browser field, a
terminal, and an elevated window (where UIPI blocks `SendInput` — and blocks paste too, so the
hotkey is simply ignored while such a window has focus); pill, tray, cue; autostart; `check`;
the two bench runs. The boxes stay unticked: it is a template for each release run, not a
record of one.

**Corpus evaluation stays private.** The maintainer's recorded corpus, references and
scoring live in the maintainer's own repo, scored by the maintainer's own tooling.
byovox ships only synthetic fixtures.

## Repository

`byovox/` — `Cargo.toml` (stable toolchain, edition 2024), `LICENSE` (MIT), `README.md`
(links the config reference), `src/`, `assets/` (the pill's font; the tray icon and the
cues are drawn and synthesised at run time), `docs/`
(platform notes, testing checklist, this spec under `docs/superpowers/specs/`).

CI is described under §Testing. The release workflow fires on a `v*` tag and publishes a
**Windows x64** zip, a `SHA256SUMS` beside it, and a build-provenance attestation over the
archive and each binary in it — minted into GitHub's attestation store, not uploaded as an
asset. Binaries are not code-signed, so SmartScreen warns on first run. Linux and macOS targets
land with the backends for those platforms; until then `cargo install --git` is the other way
in.

The three endpoint keys default to **empty**, not to placeholder hosts: a placeholder would
fail as a DNS error instead of naming the key, so `validate` refuses an empty one by name at
startup and `check` reports what is unset.

## Planned, not built

None of this exists in the tree. It is the design research behind the seams above — kept
because it is the reason the traits are shaped as they are, and because it is what a Linux or
macOS implementation would start from. Every claim here is about what an implementation *would*
do, not what byovox does.

✅ known-good pattern · ⚠️ best-effort until exercised on hardware

| Trait | Windows | Linux — Wayland (GNOME / KDE) | Linux — X11 | macOS |
|---|---|---|---|---|
| Hotkey | ✅ `WH_KEYBOARD_LL` hook on a message-pump thread; press/release for any key including a bare modifier | ✅ evdev (needs `input` group; one udev rule documented) · ⚠️ GlobalShortcuts portal via `ashpd` (`Activated`/`Deactivated`; KDE ships it, GNOME landing). **Portal shortcuts are modifier+key combos** — a bare modifier such as `ControlRight` cannot be bound, so this rung is skipped for bare-modifier keys. Needs an app id registered via `org.freedesktop.host.portal.Registry` on xdg-desktop-portal ≥ 1.21 | ✅ evdev · ⚠️ `global-hotkey` (X11) | ⚠️ `CGEventTap`, Accessibility permission |
| Capture | ✅ `cpal`/WASAPI | ✅ `cpal`/ALSA-over-PipeWire | ✅ same | ⚠️ `cpal`/CoreAudio |
| Layout | ✅ `GetForegroundWindow` → `GetWindowThreadProcessId` → `GetKeyboardLayout` → LANGID | ⚠️ GNOME: IBus D-Bus `GlobalEngine` · ⚠️ KDE: `org.kde.keyboard /Layouts getLayout` returns a **uint index**; the code comes from `getLayoutsList()[index].shortName` (`il`) | ⚠️ XKB group of focused window | ⚠️ `TISCopyCurrentKeyboardInputSource` |
| Inject | ✅ `SendInput` + `KEYEVENTF_UNICODE`; optional paste mode (save clipboard → set → Ctrl+V → restore) | ⚠️ **Clipboard + Ctrl+V through the RemoteDesktop portal** (`NotifyKeyboardKeysym` for `Control_L`/`v`, which every keymap has; one permission dialog, restore token; GNOME ≥ 46, KDE ≥ 6.1). The clipboard is set natively via `ext-data-control` where the compositor offers it (KDE), else through the **XWayland selection bridge** (an X11 client sets `CLIPBOARD`; the compositor mirrors it). ⚠️ Portal **keysym typing** only for text whose every character maps to a keysym present in the current keymap (`xkbcommon` lookup) — Mutter's Wayland path drops keysyms it cannot find. No libei | ⚠️ XTest; clipboard fallback | ⚠️ `CGEventKeyboardSetUnicodeString` |
| Indicator | ✅ tray (`tray-icon`) · ✅ pill (`winit`+`softbuffer`) · ✅ cue (`rodio`) | tray via SNI (KDE ✅, GNOME needs AppIndicator extension ⚠️) · pill off (no always-on-top; layer-shell on KDE later) · cue ✅ | ✅ all three | tray ✅ · pill ⚠️ · cue ✅ |

Why paste-first on Wayland: the portal's `NotifyKeyboardKeysym` is the only sanctioned
injection on GNOME and KDE, but Mutter's Wayland implementation injects a keysym only if
it is already in the keymap (its X11 path adds temporary keycodes; the Wayland path does
not), and layouts store language keysyms (`hebrew_aleph`), not Unicode keysyms. A
Ctrl+V chord is two keysyms every keymap has, so delivering the *text* via the clipboard
and only the *chord* via the portal is the path that cannot lose characters. Keysym
typing is kept for the case it is provably safe — every character resolves through
`xkbcommon` against the current keymap — because it avoids touching the clipboard. libei
is not used: it carries keycodes only, which is the keymap juggling every crate attempting
Wayland typing has bugs in.

**Universal fallbacks.** Hotkey → `byovox toggle` bound to an OS shortcut. Inject →
`clipboard-only`: the text is placed on the clipboard, the done cue plays, the user
pastes — where the clipboard is settable at all. Where it is not (GNOME Wayland with the
XWayland bridge unavailable) the last rung is **none**: the transcript is held in the
daemon's memory for `byovox last` (and the tray's "Show last transcript"), the error cue
plays, a desktop notification says *only* that insertion failed and how to retrieve the
text — never the text itself — and `check` reports the rung as `none` in those words.
Layout → `None` → default policy. A missing backend never fails a dictation; it degrades
one rung and logs which.

**`platform::detect()` order.** Linux hotkey: evdev → portal (only when `hotkey.key` is a
modifier+key combo) → `global-hotkey` (X11) → toggle-only. Linux inject on Wayland:
clipboard (`ext-data-control`, else XWayland selection bridge) + portal Ctrl+V → portal
keysym typing (keymap-safe text only) → clipboard-only → none. Linux inject on X11:
XTest → clipboard + XTest Ctrl+V → clipboard-only. Linux layout: KDE D-Bus if KDE, IBus
if GNOME, XKB if X11, else none. Windows and macOS have one rung each plus the universal
fallbacks.

`inject.mode` is a *preference* that `detect()` honours: `auto` (default) walks the order
above; `type`, `paste` or `clipboard-only` pins the corresponding rung and is a startup
error — naming the rungs the platform does offer — if that rung is unavailable. It never
silently becomes something else.

## Candidate crates

**In the tree:** `clap`, `serde` + `toml`, `directories`, `tracing` + `tracing-appender`,
`ureq` (with the hand-rolled multipart in `multipart.rs`), `cpal`, `hound`, `rubato`,
`tray-icon` + `muda`, `winit` + `softbuffer`, `rodio`, `arboard`, `windows` (Win32).
`Cargo.lock` is committed and every CI job builds `--locked`, the audit tools' own installs
included.

**For the platforms not built:** `evdev`, `zbus`, `ashpd` (GlobalShortcuts, RemoteDesktop),
`x11rb` (XTest, XWayland selection), `xkbcommon` (keymap-safe keysym lookup), `notify-rust`,
`objc2`/`core-foundation`. Validated at plan time, not assumed — and not depended on yet.

## Risks

The first three are about platforms that do not exist yet and are carried for whoever builds
them; the last two are live.

- **Wayland portals are young.** Two claims get verified on hardware before anything is
  built on them: that the XWayland selection bridge mirrors an X11 `CLIPBOARD` set by a
  surfaceless client into the Wayland clipboard on GNOME 50 and current KDE, and that the
  GNOME GlobalShortcuts backend delivers `Deactivated`. Each has a rung below it
  (keymap-safe keysym typing; evdev) that is v1-acceptable.
- **GNOME has no native clipboard rung.** Mutter implements neither `wlr-` nor
  `ext-data-control` (still true on GNOME 50), so a surfaceless process cannot set the
  Wayland clipboard directly, and its Wayland `NotifyKeyboardKeysym` drops keysyms absent
  from the keymap. If the XWayland bridge and keymap-safe typing both fail, GNOME's last
  rung is *none* — `byovox last` plus a text-free notification — and `check` reports
  exactly that.
- **Bare-modifier hotkeys** reach the focused app too (they cannot be swallowed on evdev)
  and cannot be bound through the GlobalShortcuts portal at all. A bare Right Ctrl does
  nothing on its own in almost every app; without `input` group membership a Linux user
  must pick a modifier+key combo or use toggle mode. Documented in the config reference.
- **macOS is unexercised.** No backend there is written at all; the seams are shaped for it and
  nothing more.
- **The endpoint types into your focused window.** Sanitising removes the keystrokes nobody
  speaks — newlines, control characters, bidi overrides — and no rung ever presses Enter, so
  nothing byovox types can submit. It cannot tell a harmful sentence from a harmless one: you
  trust that server as much as you trust your own keyboard. `SECURITY.md` is the full model.
- **The polish prompt is a measured artefact, not prose.** Its rules were each scored on the
  bench, and two of them are there because the abstract statement of the same rule was measured
  *not* to hold. Editing it by eye is how it regresses; `bench/polish_bench.py` is the gate.

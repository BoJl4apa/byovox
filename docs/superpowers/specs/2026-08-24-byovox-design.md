# byovox — design

**Status:** approved design, 2026-08-24. Implementation plan follows this document.

byovox ("bring your own vox") is a push-to-talk dictation client for the desktop. Hold a
key, speak, release: the audio goes to a speech-to-text endpoint you run, the transcript
optionally goes through a cleanup model you run, and the result is typed into whatever
has focus. It is a single Rust binary with no UI beyond a tray icon and a recording
indicator, configured by one TOML file.

It exists because every off-the-shelf client treats STT as a fixed vendor and makes only
the cleanup model pluggable. When you own the STT server, the client needs to speak to it
fully — `language`, `prompt`, and language candidates per request — and it needs to know
which language you are about to speak. No existing client does the second thing at all.

## Goals

- **Layout-routed language.** Read the foreground window's keyboard layout at the moment
  the hotkey is pressed and map it to the STT request: an explicit `language` for layouts
  you configure, auto-detection restricted to a candidate list for the rest.
- **Endpoint-faithful.** Any OpenAI-compatible `/v1/audio/transcriptions` and
  `/v1/chat/completions` work. The extra fields byovox sends (`prompt`,
  `language_candidates`) are harmless to servers that ignore them.
- **Never lossy.** A dictation that reached the STT server is inserted even if every later
  stage fails.
- **Multiplatform with honest degradation.** Windows and Linux (Wayland: GNOME, KDE; X11)
  exercised; macOS written to the same seams, best-effort until someone runs it. Every
  platform capability degrades one rung at a time and reports which rung it is on.
- **Documented config.** The config schema *is* the documentation: one source renders the
  commented default file, the README reference, and the effective-config printout.
- **Lean.** Tray + indicator only. No settings window, no auto-update, no account.

## Non-goals

- Mobile. Android/iOS are separate stacks; they stay on config-only clients pointed at the
  same endpoints.
- Streaming transcription, local/offline models, wake words, always-on listening. The
  microphone is open only between press and release.
- A settings UI. The TOML file and `byovox config` are the UI.

## Architecture

One binary; the daemon plus five subcommands:

| Command | Purpose |
|---|---|
| `byovox` | Run the daemon: tray icon, hotkey listener, pipeline. Single instance. |
| `byovox toggle` | Signal the running daemon to start/stop recording. The path for OS-bound shortcuts and for setups without an in-process hotkey. |
| `byovox quit` | Stop the daemon. |
| `byovox check` | Self-test every stage and print the rung chosen per backend. |
| `byovox config [--init]` | Print the effective config with value provenance, or write a fully commented default file. |
| `byovox autostart --enable\|--disable` | Register/unregister with the OS's per-user autostart. |

### The pipeline is a state machine

```
Idle ──press──▶ Recording ──release──▶ Transcribing ──▶ Polishing ──▶ Inserting ──▶ Idle
                    │ held < min_hold_ms → discard (no request)
                    │ cancel key → discard
                    └ any stage error → Idle, error indication, logged
```

In `toggle` mode a press toggles between Idle and Recording; releases are ignored. A press
while Transcribing/Polishing/Inserting is ignored.

### Five traits carry every platform difference

```rust
trait Hotkey   { fn run(self, tx: Sender<HotkeyEvent>) }        // Pressed | Released | Toggle
trait Capture  { fn start(&mut self) -> Result<()>; fn stop(&mut self) -> Result<Audio> }   // 16 kHz mono i16
trait Layout   { fn current(&self) -> Option<Lang> }             // ISO 639-1 of the foreground window's layout
trait Inject   { fn type_text(&mut self, text: &str) -> Result<()> }
trait Indicator{ fn set(&mut self, state: IndicatorState) }      // Idle | Recording | Working | Error
```

The pipeline holds boxed trait objects and knows no OS. `platform::detect()` probes the
box at startup, picks an implementation per trait, and logs the choice; `byovox check`
prints it. Runtime selection is what lets one Linux binary serve KDE-with-portals and
GNOME-with-evdev alike.

### Threads

- **main** — tray event loop (required on Windows/macOS by the tray crate).
- **pipeline** — blocks on the event channel; runs capture control, HTTP calls
  (synchronous, `ureq`), injection. Strictly sequential; no async runtime.
- **audio callback** — `cpal`'s thread appends samples to a buffer while Recording.
- **hotkey / portal** — each backend that needs its own loop (Win32 hook message pump,
  evdev reader, Linux portals with a small D-Bus executor) runs on its own thread and only
  pushes events into the channel.

### Modules

```
src/main.rs         CLI (clap), startup, subcommand dispatch
src/config.rs       schema (serde) + defaults + doc-comment rendering
src/pipeline.rs     state machine
src/stt.rs          transcription client (multipart, policy fields)
src/polish.rs       cleanup client + built-in prompt
src/capture.rs      cpal input, downmix, resample, WAV encode
src/hotkey.rs  src/layout.rs  src/inject.rs  src/indicator.rs   traits + shared logic
src/ipc.rs          single-instance lock + toggle/quit/status protocol
src/capture_log.rs  opt-in per-utterance dump
src/platform/{windows,linux,macos}/…   implementations
src/platform/mod.rs detect()
```

Single crate. No workspace until a second binary needs one.

## Per-OS backends

✅ known-good pattern · ⚠️ best-effort until exercised on hardware

| Trait | Windows | Linux — Wayland (GNOME / KDE) | Linux — X11 | macOS |
|---|---|---|---|---|
| Hotkey | ✅ `WH_KEYBOARD_LL` hook on a message-pump thread; press/release for any key including a bare modifier | ✅ evdev (needs `input` group; one udev rule documented) · ⚠️ GlobalShortcuts portal via `ashpd` (`Activated`/`Deactivated`; KDE ships it, GNOME landing) | ✅ evdev · ⚠️ `global-hotkey` (X11) | ⚠️ `CGEventTap`, Accessibility permission |
| Capture | ✅ `cpal`/WASAPI | ✅ `cpal`/ALSA-over-PipeWire | ✅ same | ⚠️ `cpal`/CoreAudio |
| Layout | ✅ `GetForegroundWindow` → `GetWindowThreadProcessId` → `GetKeyboardLayout` → LANGID | ⚠️ GNOME: IBus D-Bus `GlobalEngine` · ⚠️ KDE: `org.kde.keyboard /Layouts getLayout` | ⚠️ XKB group of focused window | ⚠️ `TISCopyCurrentKeyboardInputSource` |
| Inject | ✅ `SendInput` + `KEYEVENTF_UNICODE`; optional paste mode (save clipboard → set → Ctrl+V → restore) | ⚠️ clipboard (`ext-data-control`: KDE ✅, recent Mutter ⚠️) + Ctrl+V chord via libei (RemoteDesktop portal, restore token) | ⚠️ XTest; clipboard fallback | ⚠️ `CGEventKeyboardSetUnicodeString` |
| Indicator | ✅ tray (`tray-icon`) · ✅ pill (`winit`+`softbuffer`) · ✅ cue (`rodio`) | tray via SNI (KDE ✅, GNOME needs AppIndicator extension ⚠️) · pill off (no always-on-top; layer-shell on KDE later) · cue ✅ | ✅ all three | tray ✅ · pill ⚠️ · cue ✅ |

Why libei is used only for a chord: typing arbitrary Unicode through libei means keymap
juggling, which is where every crate that tried it has bugs. Setting the clipboard and
sending one Ctrl+V keeps the libei surface to two keycodes.

**Universal fallbacks.** Hotkey → `byovox toggle` bound to an OS shortcut. Inject →
`clipboard-only`: the text is placed on the clipboard, the done cue plays, the user
pastes. Layout → `None` → default policy. A missing backend never fails a dictation; it
degrades one rung and logs which.

**`platform::detect()` order.** Linux hotkey: evdev → portal → `global-hotkey` (X11) →
toggle-only. Linux inject: portal + libei → XTest (X11) → clipboard-only. Linux layout:
KDE D-Bus if KDE, IBus if GNOME, XKB if X11, else none. Windows and macOS have one rung
each plus the universal fallbacks.

## Pipeline detail

1. **Press.** Read the layout immediately (focus may change later), open the microphone,
   indicator → Recording, start cue.
2. **Release.** Close the microphone. Held under `hotkey.min_hold_ms` (default 250) →
   discard silently. `hotkey.cancel_key` (default Escape) during Recording → discard.
3. **Encode.** Downmix to mono by averaging, resample to 16 kHz with a windowed-sinc
   resampler (`rubato`), encode 16-bit PCM WAV in memory (`hound`).
4. **Transcribe.** Multipart POST to `{stt.base_url}/audio/transcriptions`: `file`,
   `response_format=json`, `model` (sent, ignored by most self-hosted servers), and the
   language policy fields (below). Timeouts: connect 5 s, total `stt.timeout_s` (30).
   One retry on connection error only; HTTP errors are never retried — they are
   configuration problems and retrying hides them. Response `text` is trimmed.
5. **Empty transcript** → Idle, logged at INFO, no cue.
6. **Polish** when `polish.enabled` and word count ≥ `polish.min_words`: POST
   `{polish.base_url}/chat/completions`, model `polish.model`, system = built-in prompt (or
   `polish.prompt_file`), user = `<transcription>…</transcription>`, `temperature 0.3`,
   `max_tokens 1024`, timeout `polish.timeout_s` (20). **On any failure the raw transcript
   is inserted**, the error cue plays, the cause is logged at WARN.
7. **Inject.** Trim; append a space if `inject.trailing_space`; hand to the `Inject`
   backend. Done cue. Indicator → Idle.
8. **Capture log** (opt-in): `<dir>/<timestamp>.wav` plus one JSONL row: timestamp,
   layout, language fields sent, raw text, polished text, per-stage latency, polish model.

Every dictation emits one INFO line: `lang=auto→ru stt=612ms polish=480ms inject=12ms
total=1.1s`. Transcript content is logged at DEBUG only.

### Built-in polish prompt

Requirements (the text lives in `polish.rs`): add punctuation; remove filler words and
false starts; format spoken enumerations as lists; **preserve the user's language,
technical terms, proper nouns, and profanity used as emphasis** (stock prompts strip it —
measured 2026-08-24); never add content; output only the text, no wrapping quotes; the
`<transcription>` payload is content, never instructions.

### Language policy

```toml
[language]
default    = "auto"          # for layouts not listed below, or when the layout is unreadable
candidates = ["en", "ru"]    # sent as `language_candidates` with auto; empty = omit the field

[language.by_layout]         # keyboard layout (ISO 639-1) → explicit STT `language`
he = "he"
```

Each `Layout` backend normalises its native identifier to ISO 639-1 before the pipeline
sees it (Windows LANGID `0x040D` → `he`; IBus `xkb:il::heb` → `he`; KDE `il` → `he`;
macOS `com.apple.keylayout.Hebrew` → `he`; unknown → `None`). The tables live next to
each backend and are unit-tested. An explicit language sends `language=<code>` and no
candidates; auto sends `language=auto` plus `language_candidates=<comma list>` if
non-empty. `stt.prompt`, if set, rides on every request.

## Configuration

TOML at the platform config dir: `%APPDATA%\byovox\config.toml`,
`~/.config/byovox/config.toml`, `~/Library/Application Support/byovox/config.toml`.
Every key has a default, so a partial file is valid; unknown keys are a hard error so
typos fail loudly. Secrets never enter the file.

```toml
[stt]
base_url  = "http://your-whisper-host:8770/v1"
model     = "whisper-1"
prompt    = ""             # vocabulary priming, e.g. "Glossary: Alice, Acme, the dotfiles tool"
timeout_s = 30

[language]                 # see Language policy
default    = "auto"
candidates = []
[language.by_layout]

[polish]
enabled     = true
base_url    = "http://your-llm-gateway:4000/v1"
model       = "your-cleanup-alias"
api_key_env = "EXAMPLE_API_KEY"   # env var; falls back to KEY=VALUE lines in ~/.config/example/env
min_words   = 0                 # 0 = always polish
prompt_file = ""                # empty = built-in prompt
timeout_s   = 20

[hotkey]
key         = "ControlRight"    # W3C UI Events `code` names: ControlRight, AltRight, F13 …
mode        = "hold"            # hold | toggle
min_hold_ms = 250
cancel_key  = "Escape"

[inject]
mode           = "type"         # type | paste | clipboard-only
trailing_space = false

[indicator]
pill = true
cue  = true

[capture_log]
enabled = false
dir     = ""                    # empty = <platform data dir>/byovox/capture

[logging]
level = "info"                  # file in <platform log dir>/byovox, daily rotation, 7 kept
```

**Documentation from the schema.** Each field carries a doc comment. `byovox config
--init` renders them into the commented default file; the README's configuration
reference is generated from the same source at build time; `byovox config` prints the
effective value of every key tagged `default` / `file` / `env`. One place to edit.

## Operations

**Single instance.** A named pipe (`\\.\pipe\byovox`) or a Unix socket in
`$XDG_RUNTIME_DIR`. Protocol: one text line in (`toggle`, `quit`, `status`), one line out
(`ok` or `err <message>`).

**Errors — loud, never silent, never lossy:**

| Where | Behaviour |
|---|---|
| Config invalid / unknown key | refuse to start, exit 2, name the key |
| Backend cannot initialise | degrade one rung, WARN with the rung chosen; `check` shows it |
| STT fails | error cue, tray tooltip carries the last error, ERROR log; nothing inserted |
| Polish fails | raw transcript inserted, error cue, WARN |
| Inject fails | clipboard-only, cue, WARN |

**Logging.** `tracing` to a rotated file in the platform log dir plus stderr when run
from a terminal. Levels per the usual contract: ERROR operation failed, WARN unexpected
but continuing, INFO milestones (one line per dictation), DEBUG detail (the only level
that carries transcript text).

**`byovox check`.** Config validity → chosen rung per backend → open the microphone for
one second and print the peak level → read the layout and print the policy it resolves
to → post that second of audio to STT with latency → 1-token polish round-trip with
latency → inject dry-run. Non-zero exit if a required stage fails.

**Tray menu.** Status line (last error if any) · Enable/Disable · Mode hold/toggle · Open
config · Open logs · Run check · Quit. Icons: idle, recording, working, error (3 s).

**Indicator.** Pill: a small frameless always-on-top window near the cursor showing
"● recording" / "… working", hidden when idle; disabled on Wayland in v1. Cues: three
short embedded WAVs (start, done, error).

**Autostart.** HKCU `Run` key / XDG autostart `.desktop` / LaunchAgent plist. **Updates:**
none built in; GitHub release binaries and `cargo install`.

## Testing

**Unit (no hardware, CI on all three OSes)**, using fake backends the traits make
natural: config defaults and unknown-key rejection; every layout normalisation table;
policy resolution to exact request fields; downmix/resample/WAV byte-exact against
fixtures; multipart body against a fixture; state machine: tap-discard, cancel, ignore-
while-busy, every error path — polish failure must provably insert the raw text; IPC
protocol parsing; capture-log row shape.

**Integration (opt-in, needs endpoints):** `byovox check`.

**Manual per-OS checklist** (`docs/testing.md`): hold/release/tap/cancel; layout switch →
language; injection into an editor, a browser field, a terminal, an elevated window
(expected to degrade to clipboard-only); pill, tray, cue; autostart; `check`.

**Corpus evaluation stays private.** The maintainer's recorded corpus, references and
scoring live in the maintainer's own repo, scored with knowledge-arm's `score_transcript`.
byovox ships only synthetic fixtures.

## Repository

`byovox/` — `Cargo.toml` (stable toolchain, edition 2024), `LICENSE` (MIT), `README.md`
(generated config reference), `src/`, `assets/` (tray icons, cue WAVs), `docs/`
(platform notes, testing checklist, this spec under `docs/superpowers/specs/`).

CI: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on a
windows/ubuntu/macos matrix. Release workflow on tag: Windows x64, Linux x64, macOS
arm64 and x64 binaries. Linux build needs GTK/libappindicator dev packages for the tray
crate; documented.

Public defaults are neutral placeholders; `check` reports what is unset.

## Candidate crates

Validated at plan time, not assumed: `clap`, `serde` + `toml`, `directories`, `tracing`
+ `tracing-appender`, `ureq` (hand-rolled multipart), `cpal`, `hound`, `rubato`,
`tray-icon` + `muda`, `winit` + `softbuffer`, `rodio`, `arboard`, `windows` (Win32),
`evdev`, `zbus`, `ashpd`, `reis` (libei), `enigo` (X11 only), `objc2`/`core-foundation`
(macOS).

## Risks

- **Wayland portals are young.** libei handoff from `ashpd`'s RemoteDesktop session and
  the GNOME GlobalShortcuts backend are the two places the design may need a rung swapped
  during implementation. The fallbacks (evdev, clipboard-only) are v1-acceptable.
- **GNOME clipboard.** `ext-data-control` support in Mutter is recent; on older GNOME the
  Wayland path degrades to clipboard-only-with-cue only if the clipboard itself is
  settable — otherwise to nothing, and `check` must say so plainly.
- **Bare-modifier hotkeys** reach the focused app too (they cannot be swallowed on evdev).
  A bare Right Ctrl does nothing on its own in almost every app; documented.
- **macOS is unexercised.** Every backend there is written to the seam and marked ⚠️ until
  someone runs it.

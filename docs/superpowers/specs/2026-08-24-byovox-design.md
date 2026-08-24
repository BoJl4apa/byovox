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
  `/v1/chat/completions` work. `language` and `prompt` are standard fields.
  `language_candidates` is a whisper.cpp-server extension (the maintainer's fork; upstream
  PR pending) — servers that lack it ignore the field and fall back to unconstrained
  auto-detection, which is what they would have done anyway.
- **Never lossy.** A dictation that reached the STT server is inserted even if every later
  stage fails.
- **Multiplatform with honest degradation.** Windows and Linux on KDE Plasma (Wayland)
  are exercised for v1; GNOME, X11 and macOS are written to the same seams and marked
  best-effort until someone runs them. Every platform capability degrades one rung at a
  time and reports which rung it is on.
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
| `byovox last` | Print the most recent transcript held by the daemon (memory only, cleared on quit) — the retrieval path when no inject rung worked. |
| `byovox check` | Self-test every stage and print the rung chosen per backend. |
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

## Pipeline detail

1. **Press.** Read the layout immediately (focus may change later), open the microphone,
   indicator → Recording, start cue.
2. **Release.** Close the microphone. Held under `hotkey.min_hold_ms` (default 250) →
   discard silently. `hotkey.cancel_key` (default Escape) during Recording → discard.
3. **Encode.** Downmix to mono by averaging, resample to 16 kHz with a windowed-sinc
   resampler (`rubato`), encode 16-bit PCM WAV in memory (`hound`).
4. **Transcribe.** Multipart POST to `{stt.base_url}/audio/transcriptions`: `file`,
   `response_format=json`, `model` (sent, ignored by most self-hosted servers), the
   language policy fields (below), and `Authorization: Bearer` when `stt.api_key_env`
   resolves. Timeouts: connect 5 s, total `stt.timeout_s` (30).
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
total=1.1s`. Transcript content is logged at DEBUG only — this holds on every path,
including the `none` inject rung, which logs the failure at WARN without the text.

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
sees it (Windows LANGID `0x040D` → `he`; IBus `xkb:il::heb` → `he`; KDE
`getLayoutsList()[getLayout()].shortName` = `il` → `he`; macOS
`com.apple.keylayout.Hebrew` → `he`; unknown → `None`). The tables live next to each
backend and are unit-tested.

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
Every key has a default, so a partial file is valid; unknown keys are a hard error so
typos fail loudly. Secrets never enter the file: both stages take a bearer token from an
environment variable named in the config, and polish may additionally name a
`KEY=VALUE` file to read when the variable is unset (the maintainer's deployment points
it at the same file the rest of that toolchain uses). Token values are never logged.

```toml
[stt]
base_url    = "http://your-whisper-host:8770/v1"
model       = "whisper-1"
api_key_env = ""           # env var holding a bearer token; empty = no Authorization header
prompt      = ""           # vocabulary priming, e.g. "Glossary: Alice, Acme, the dotfiles tool"
timeout_s   = 30

[language]                 # see Language policy
default    = "auto"
candidates = []
[language.by_layout]

[polish]
enabled     = true
base_url    = "http://your-llm-gateway:4000/v1"
model       = "your-cleanup-alias"
api_key_env = ""                # env var holding the bearer token
api_key_file = ""               # optional KEY=VALUE file consulted when the env var is unset
min_words   = 0                 # 0 = always polish
prompt_file = ""                # empty = built-in prompt
timeout_s   = 20

[hotkey]
key         = "ControlRight"    # W3C UI Events `code` names: ControlRight, AltRight, F13 …
mode        = "hold"            # hold | toggle
min_hold_ms = 250
cancel_key  = "Escape"

[inject]
mode           = "auto"         # auto | type | paste | clipboard-only — see detect()
trailing_space = false

[indicator]
pill = true
cue  = true

[capture_log]
enabled = false
dir     = ""                    # empty = byovox's data dir + capture, e.g. %APPDATA%\byovox\data\capture

[logging]
level = "info"                  # file in <platform log dir>/byovox, daily rotation, 7 kept
```

**Documentation from the schema.** Each field carries a doc comment. `byovox config
--init` renders them into the commented default file; the README's configuration
reference is generated from the same source at build time; `byovox config` prints the
effective value of every key tagged `default` / `file` / `env`. One place to edit.

## Operations

**Single instance.** A named pipe (`\\.\pipe\byovox`) or a Unix socket in
`$XDG_RUNTIME_DIR`. Protocol: one JSON object per line each way. Request
`{"cmd": "toggle" | "quit" | "status" | "last"}`; reply `{"ok": true, ...}` or
`{"ok": false, "error": "<message>"}`. `status` replies with the pipeline state and the
last error; `last` replies `{"ok": true, "text": "<transcript>"}` — JSON escaping
carries newlines, so a polished list arrives intact — or `ok: false` when nothing is
held. Anything unparseable gets `ok: false` and the connection closes.

**Errors — loud, never silent, never lossy:**

| Where | Behaviour |
|---|---|
| Config invalid / unknown key | refuse to start, exit 2, name the key |
| Backend cannot initialise | degrade one rung, WARN with the rung chosen; `check` shows it |
| STT fails | error cue, tray tooltip carries the last error, ERROR log; nothing inserted |
| Polish fails | raw transcript inserted, error cue, WARN |
| Inject fails | next inject rung, down to `none`: transcript held for `byovox last`, error cue, notification without the text, WARN (event only, no content) |

**Logging.** `tracing` to a rotated file in the platform log dir plus stderr when run
from a terminal. Levels per the usual contract: ERROR operation failed, WARN unexpected
but continuing, INFO milestones (one line per dictation), DEBUG detail (the only level
that carries transcript text).

**`byovox check`.** Config validity → chosen rung per backend → open the microphone for
one second and print the peak level → read the layout and print the policy it resolves
to → post that second of audio to STT with latency → 1-token polish round-trip with
latency → inject dry-run. Non-zero exit if a required stage fails.

**Tray menu.** Status line (last error if any) · Enable/Disable · Mode hold/toggle · Show
last transcript · Open config · Open logs · Run check · Quit. Icons: idle, recording,
working, error (3 s).

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
(expected to degrade one rung — on Windows UIPI blocks `SendInput` into elevated windows,
so paste is blocked too and the outcome is clipboard-only); the `none` rung on GNOME
Wayland with `byovox last` retrieval; pill, tray, cue; autostart; `check`.

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
`evdev`, `zbus`, `ashpd` (GlobalShortcuts, RemoteDesktop), `x11rb` (XTest, XWayland
selection), `xkbcommon` (keymap-safe keysym lookup), `notify-rust`,
`objc2`/`core-foundation` (macOS).

## Risks

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
- **macOS is unexercised.** Every backend there is written to the seam and marked ⚠️ until
  someone runs it.

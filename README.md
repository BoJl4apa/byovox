# byovox

[![ci](https://img.shields.io/github/actions/workflow/status/BoJl4apa/byovox/ci.yml?branch=main&label=ci)](https://github.com/BoJl4apa/byovox/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/BoJl4apa/byovox)](https://github.com/BoJl4apa/byovox/releases)
[![license](https://img.shields.io/github/license/BoJl4apa/byovox)](LICENSE)

Push-to-talk dictation against a speech-to-text server **you** run: hold a key, speak, release,
and the transcript — optionally cleaned up by a language model you also run — is typed into the
focused window. The language comes from the **keyboard layout of the window you are in**, read
at the moment you press the key.

## Is this for you?

**You need**

- A **whisper-compatible STT server you can reach.** whisper.cpp's `whisper-server` is the
  reference; anything serving OpenAI's `/v1/audio/transcriptions` works. `language_candidates`
  needs a build that understands the field — one without it auto-detects unconstrained.
- Optionally, an **OpenAI-compatible `/v1/chat/completions` endpoint** for cleanup —
  punctuation, filler removal, spoken lists, and glossary terms back in Latin script when
  whisper wrote them in Hebrew or Cyrillic. `polish.enabled = false` skips the stage.
- **Windows 10 or 11.** Linux (KDE Wayland first) and macOS are designed, not implemented; the
  per-OS backend table is in [the design](docs/superpowers/specs/2026-08-24-byovox-design.md).

**You get**

- Hold a key, speak, release; the text lands in the focused window — any window, no plugin.
- The language routed from that window's keyboard layout: switch layout, and dictation language
  switches with it, with nothing configured per app.
- Cleanup that **falls back to the raw transcript** whenever it fails, for any reason.
- **Never lossy:** a dictation that reached the server is inserted even if every later stage
  fails; if no injection path works at all, `byovox last` hands the text back.
- A tray icon, a recording pill, three short cues — no settings window, no auto-update, no
  telemetry — and one TOML file, which holds no token: only the name of a variable, or the path
  of a file, to read one from.

## Quick start

### 1. A speech-to-text server

Build or download `whisper-server` from [whisper.cpp](https://github.com/ggml-org/whisper.cpp)
and run it with a model you have:

```sh
whisper-server -m ggml-large-v3-turbo.bin --host 127.0.0.1 --port 8770 -l auto --convert
```

`-l auto` is the flag to get right. `-l` names the spoken language and the server's own default
is `en` — but a forced language that does not match the speech does not fail loudly: what comes
back is text in the forced language, so Russian spoken under `-l en` returns English.
(Deliberate translation is `--translate`, a separate flag; byovox leaves it off.) `auto` is also
what makes byovox's policy mean anything, since byovox sends *no* `language` field when the
policy resolves to auto and the server's default then decides. (`--convert` needs ffmpeg;
byovox posts 16 kHz mono WAV already, so it is optional here.)

### 2. Optionally, a cleanup model

Anything serving `/v1/chat/completions` — llama.cpp's `llama-server`, Ollama, a LiteLLM
gateway. Keep it resident: a model warm between dictations is the difference between half a
second and five.

```sh
llama-server -m <a-small-instruct-model>.gguf --host 127.0.0.1 --port 4000
```

### 3. Install byovox

**Download** the Windows x86_64 archive from the
[Releases page](https://github.com/BoJl4apa/byovox/releases) — both binaries sit at the top of
the zip and must stay together — and verify it first:

```sh
sha256sum -c SHA256SUMS                                              # published beside the zip
gh attestation verify byovox-v0.1.2-x86_64-pc-windows-msvc.zip --owner BoJl4apa
```

`sha256sum` wants Git Bash or WSL; from PowerShell use `Get-FileHash -Algorithm SHA256` and
compare by eye. `gh attestation verify` is GitHub's build provenance: it says this archive came
out of this repository's release workflow and names the commit it was built from, the one
`byovox --version` prints. The binaries are **not code-signed**, so SmartScreen warns on first
run (More info → Run anyway).

**Or build it** — Rust 1.88 or newer, no SmartScreen warning, the binary being your own (from a
checkout: `cargo install --path . --locked`):

```sh
cargo install --git https://github.com/BoJl4apa/byovox --locked  # ~2 min; byovox + byovox-daemon
```

### 4. Point it at your servers

Run `byovox setup` to be asked for the endpoints — it probes each answer against your server as
you give it, writes the commented config file, and finishes with `byovox check`. Or by hand:
`byovox config --init` writes the documented default file and prints its path; open it and set
three values — byovox refuses to start the daemon or run `check` while they are empty, and names
the missing key — then the two keys byovox exists for:

```toml
[stt]
base_url = "http://127.0.0.1:8770/v1"   # byovox posts to {base_url}/audio/transcriptions
[polish]
base_url = "http://127.0.0.1:4000/v1"   # {base_url}/chat/completions
model    = "the name your gateway answers to"
[language]
candidates = ["en", "ru"]               # sent as `language_candidates` while auto is in force
[language.by_layout]                    # keyboard layout → an explicit STT language
he = "he"
[stt.by_language.he]                    # that language's own endpoint, e.g. a Hebrew fine-tune
base_url = "http://127.0.0.1:8770/he/v1"
```

`stt.model` (default `whisper-1`) is sent and ignored by self-hosted whisper.cpp; a hosted API
needs a real name. `stt.by_language.<code>` gives one language its own endpoint, used only
when the layout routing (or `language.default`) resolved that language — never on
auto-detect. It is retried on `[stt]` when it answers with no text; an error from it is an
error, named `stt[he]` so you know which server to look at. `stt.api_key_env` and
`polish.api_key_env` name an *environment variable* to read a bearer token from; an endpoint
authenticating by network identity needs neither.

### 5. Check it, then dictate

```
$ byovox check
ok   config    C:\Users\you\AppData\Roaming\byovox\config\config.toml
ok   hotkey    ControlRight hold, cancel Escape
ok   backends  hotkey=hook layout=win32 inject=type,paste,clipboard-only
ok   mic       Microphone Array (48000 Hz, 2 ch, F32)  peak -21.7 dBFS over 0.9s
     layout    en → language_candidates=en,ru
ok   stt       0.34s  p_nospeech=0.71 (would be dropped as silence)  "Thank you."
ok   stt[he]   0.50s  p_nospeech=0.00  ""
ok   polish    0.46s  "So this is a test."
     inject    dry-run: would use `type` first

all required stages passed
```

`check` records one second of room tone, so a high `p_nospeech` beside an invented stock phrase
is the healthy result, not a broken server. A `mic` peak under −40 dBFS says so outright — a
muted device, or a Windows audio enhancement attenuating it. Then:

```sh
byovox         # starts the daemon in the background and prints its pid: tray icon, hotkey live
```

Hold **Right Ctrl**, speak, release; the text lands in the focused window. Escape while holding
discards it, and a hold under 250 ms is a tap that never leaves the machine. Quit from the tray
or with `byovox quit`. The daemon is `byovox-daemon`, a windowless binary beside the CLI, so the
tray outlives the terminal you started it from; `byovox run` keeps it here instead, logging to
stderr as well as to the file — that is how you watch one that will not come up.

## Commands

| | |
|---|---|
| `byovox` | start the daemon in the background: hotkey, tray icon, indicator |
| `byovox run` | run it in this terminal instead, logging to stderr too |
| `byovox setup` | interactive first-run: asks for the endpoints, probes them, writes the config |
| `byovox check` | self-test — every stage, and which backend each one chose |
| `byovox config` | print the effective configuration and where every key came from; `--init` writes the documented default file instead (never overwriting one) |
| `byovox status` | pipeline state and last error of the running daemon |
| `byovox last` | print the most recent transcript (the one held if no rung worked) |
| `byovox toggle` | start or stop a recording in the running daemon (two calls = one dictation) |
| `byovox quit` | stop the daemon |
| `byovox autostart --enable` / `--disable` | per-user autostart |

`--config <path>` points the ones that read it at a different file.

## Why this exists

Three languages, layouts switched dozens of times a day. Hosted dictation infers the language
from the audio alone, and across a close set it gets it wrong often enough to matter — the
failure being not a garbled word but a whole sentence coming back in the wrong language rather
than transcribed. The self-hosted client tried first solved the privacy half and not this one:
it had no way to send an explicit `language`, a candidate list, or a `prompt` carrying the
names and jargon whisper otherwise mangles, and read its config once at start. So byovox
sends all three per request and reads the keyboard layout to decide them — the one signal
that already knows, per window, which language you are about to speak.
The rest is warmth and locality: models resident on a machine on your own network mean nothing
you say crosses the internet, and the second dictation is not slower than the first.

That could have been a fork rather than a new client. It was not: the closest candidate held
its configuration in memory with no runtime path to change it, auto-updated over any patch, and
had no `prompt` field to extend — three fights per upstream release, for a feature list smaller
than this README. The full argument, and what was measured to get there, is in
[the design doc](docs/superpowers/specs/2026-08-24-byovox-design.md).

## How it compares

| | byovox | [Wispr Flow](https://wisprflow.ai/) | [Windows Voice Typing](https://support.microsoft.com/en-us/windows/use-voice-typing-to-talk-instead-of-type-on-your-pc-fec94565-c4bd-329d-e59a-af033fa5689f) (Win+H) | [OpenTypeless](https://github.com/tover0314-w/opentypeless) |
|---|---|---|---|---|
| Runs against a server you control | the only mode | ? | no — Azure Speech services | yes — local Whisper-compatible endpoint |
| Open source | MIT | ? | no — a component of Windows | MIT |
| Language routed from the window's keyboard layout | yes | ? | follows the input language you switch with Win+Space | ? |
| Constrained auto-detect (candidate list) | yes — `language_candidates` | yes — selecting languages narrows the set Flow chooses from | ? | ? |
| Works offline / on a private network | yes | ? | no — requires an internet connection | yes — runs without OpenTypeless cloud services |
| Platform | Windows | Mac, Windows, iPhone, Android | Windows 10, Windows 11 | Windows, macOS, Linux |

**?** — that project's own public documentation does not say, and this table does not guess.
Every other cell comes from the linked pages, read 2026-08-26 — except that Windows Voice
Typing ships as part of Windows and has no source release.

## Security model

byovox watches the keyboard, opens the microphone and types into your windows, so three
things are worth knowing before you install it. The full threat model — what is in scope,
what is not — is in [SECURITY.md](SECURITY.md), along with how to report a vulnerability.

- **The endpoint you configure can type into your windows.** Whatever the STT and polish
  servers return is sent as real keystrokes. A newline becomes a space (never an Enter), control
  characters and bidi overrides are stripped before anything is typed, and a transcript over
  `inject.max_chars` is held rather than typed — but the *text* is still the server's to
  choose. Point byovox at servers you run or trust, and prefer `localhost`, a
  WireGuard/Tailscale link, or `https://` over plain HTTP — the token rides on the request, and
  a non-loopback `http://` endpoint gets a `warn network` row from `byovox check`.
- **The capture log stores your voice and your text in plain files.** It is off by default;
  `capture_log.enabled = true` writes a WAV and the transcript for every dictation, kept for
  `capture_log.keep_days` (30 by default, `0` for ever). Transcripts also reach the log file
  at `logging.level = "debug"`.
- **The hook sees every key but records none.** Key events are compared against your hotkey
  and cancel key and are never logged, stored or sent anywhere; only a chord's trigger is
  swallowed. Anything running as you could do the same — byovox draws no boundary there.

## Documentation

[`docs/config.example.toml`](docs/config.example.toml) is the configuration reference: every key
at its default with the reason it exists, every accepted hotkey name, the chord syntax.
[`docs/platform-windows.md`](docs/platform-windows.md) covers chords, elevated windows,
microphone level and autostart; [`docs/testing.md`](docs/testing.md) is the manual checklist run
before a release tag.

License: MIT.

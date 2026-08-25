# byovox

Push-to-talk dictation against a speech-to-text server **you** run. Hold a key, speak,
release — the transcript (optionally cleaned up by a language model you also run) is
typed into whatever has focus.

What makes it different: it reads the **keyboard layout of the window you're in** when
you press the key and routes the language from it — an explicit language for layouts you
map, constrained auto-detection for the rest. Multilingual dictation without touching a
setting.

- One TOML file (secrets via environment variables), a tray icon and a recording indicator.
  Nothing else.
- Any OpenAI-compatible `/v1/audio/transcriptions` and `/v1/chat/completions`.
  Constrained auto-detection (`language_candidates`) needs a whisper.cpp server that
  understands the field; everything else is standard.
- Windows is the platform that is exercised end to end today. Linux (KDE Wayland first) and
  macOS are designed, not implemented — the per-OS backend table is in
  [the design](docs/superpowers/specs/2026-08-24-byovox-design.md), under "Per-OS backends".
- A dictation that reached the server is never lost: if cleanup fails, the raw text is
  typed instead; if no injection path works on your desktop, `byovox last` hands it back.

## Quick start

Rust 1.88 or newer.

```sh
cargo install --path .   # installs both binaries, byovox and byovox-daemon, side by side
                         # (or: cargo build --release, then use target/release)
byovox config --init     # writes the documented default file and prints its path
```

Open that file and set the four endpoint keys — `stt.base_url`, `stt.model`,
`polish.base_url`, `polish.model` — to your own servers. Tokens are never written here:
`stt.api_key_env` and `polish.api_key_env` name an *environment variable* to read one from,
and an endpoint that authenticates by network identity needs neither. Set
`polish.enabled = false` to insert the raw transcript and skip the cleanup stage entirely.

```sh
byovox check   # exercises every stage — mic, layout, STT, polish — and reports the rungs
byovox         # starts the daemon in the background and prints its pid: tray icon, hotkey live
```

The daemon is `byovox-daemon`, a windowless binary beside the CLI: the tray outlives the
terminal you started it from. `byovox run` keeps it in this terminal instead, logging to
stderr as well as to the file — that is how you watch one that will not come up.

Hold **Right Ctrl**, speak, release; the text lands in the focused window. Escape while
holding discards the recording. Quit from the tray, or `byovox quit`.

## Commands

| | |
|---|---|
| `byovox` | start the daemon in the background: hotkey, tray icon, indicator |
| `byovox run` | run it in this terminal instead, logging to stderr too |
| `byovox check` | self-test — every stage, and which backend each one chose |
| `byovox config` | print the effective configuration and where every key came from |
| `byovox config --init` | write the documented default file (never overwrites one) |
| `byovox status` | pipeline state and last error of the running daemon |
| `byovox last` | print the most recent transcript (the one held if no rung worked) |
| `byovox toggle` | start or stop a recording in the running daemon (two calls = one dictation) |
| `byovox quit` | stop the daemon |
| `byovox autostart --enable` / `--disable` | per-user autostart |

`--config <path>` points any of them at a different file.

## Security model

byovox watches the keyboard, opens the microphone and types into your windows, so three
things are worth knowing before you install it. The full threat model — what is in scope,
what is not — is in [SECURITY.md](SECURITY.md), along with how to report a vulnerability.

- **The endpoint you configure can type into your windows.** Whatever the STT and polish
  servers return is sent as real keystrokes, and a newline is a real Enter. Control
  characters and bidi overrides are stripped before anything is typed, and a transcript over
  `inject.max_chars` is held rather than typed — but the *text* is still the server's to
  choose. Point byovox at servers you run or trust, and prefer `localhost`, a
  WireGuard/Tailscale link, or `https://` over plain HTTP — the token rides on the request.
- **The capture log stores your voice and your text in plain files.** It is off by default;
  `capture_log.enabled = true` writes a WAV and the transcript for every dictation, and
  prunes nothing. Transcripts also reach the log file at `logging.level = "debug"`.
- **The hook sees every key but records none.** Key events are compared against your hotkey
  and cancel key and are never logged, stored or sent anywhere; only a chord's trigger is
  swallowed. Anything running as you could do the same — byovox draws no boundary there.

## Documentation

- [`docs/config.example.toml`](docs/config.example.toml) — the configuration reference: every
  key at its default, with the reason it exists. `config --init` writes exactly this file.
- [`docs/platform-windows.md`](docs/platform-windows.md) — bare-modifier hotkeys and chords,
  elevated windows, microphone level, autostart, where config and logs live.
- [`docs/testing.md`](docs/testing.md) — the manual checklist, run before a release tag.
- [`SECURITY.md`](SECURITY.md) — the threat model, and how to report a vulnerability.
- [`docs/superpowers/specs/2026-08-24-byovox-design.md`](docs/superpowers/specs/2026-08-24-byovox-design.md)
  — the design.

License: MIT.

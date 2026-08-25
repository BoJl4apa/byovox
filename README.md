# byovox

Push-to-talk dictation against a speech-to-text server **you** run. Hold a key, speak,
release — the transcript (optionally cleaned up by a language model you also run) is
typed into whatever has focus.

What makes it different: it reads the **keyboard layout of the window you're in** when
you press the key and routes the language from it — an explicit language for layouts you
map, constrained auto-detection for the rest. Multilingual dictation without touching a
setting.

- One binary, one TOML file (secrets via environment variables), a tray icon and a
  recording indicator. Nothing else.
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
cargo install --path .   # or: cargo build --release, then use target/release/byovox
byovox config --init     # writes the documented default file and prints its path
```

Open that file and set the four endpoint keys — `stt.base_url`, `stt.model`,
`polish.base_url`, `polish.model` — to your own servers. Tokens are never written here:
`stt.api_key_env` and `polish.api_key_env` name an *environment variable* to read one from,
and an endpoint that authenticates by network identity needs neither. Set
`polish.enabled = false` to insert the raw transcript and skip the cleanup stage entirely.

```sh
byovox check   # exercises every stage — mic, layout, STT, polish — and reports the rungs
byovox         # the daemon: tray icon, hotkey live
```

Hold **Right Ctrl**, speak, release; the text lands in the focused window. Escape while
holding discards the recording. Quit from the tray, or `byovox quit`.

## Commands

| | |
|---|---|
| `byovox` | run the daemon: hotkey, tray icon, indicator |
| `byovox check` | self-test — every stage, and which backend each one chose |
| `byovox config` | print the effective configuration and where every key came from |
| `byovox config --init` | write the documented default file (never overwrites one) |
| `byovox status` | pipeline state and last error of the running daemon |
| `byovox last` | print the most recent transcript (the one held if no rung worked) |
| `byovox toggle` | start or stop a recording in the running daemon (two calls = one dictation) |
| `byovox quit` | stop the daemon |
| `byovox autostart --enable` / `--disable` | per-user autostart |

`--config <path>` points any of them at a different file.

## Documentation

- [`docs/config.example.toml`](docs/config.example.toml) — the configuration reference: every
  key at its default, with the reason it exists. `config --init` writes exactly this file.
- [`docs/platform-windows.md`](docs/platform-windows.md) — bare-modifier hotkeys, elevated
  windows, microphone level, autostart, where config and logs live.
- [`docs/testing.md`](docs/testing.md) — the manual checklist, run before a release tag.
- [`docs/superpowers/specs/2026-08-24-byovox-design.md`](docs/superpowers/specs/2026-08-24-byovox-design.md)
  — the design.

License: MIT.

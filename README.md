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
- Windows and Linux (Wayland and X11); macOS best-effort.
- A dictation that reached the server is never lost: if cleanup fails, the raw text is
  typed instead; if no injection path works on your desktop (possible on GNOME
  Wayland), `byovox last` hands it back.

Status: **design complete, implementation starting.** The design is in
[`docs/superpowers/specs/2026-08-24-byovox-design.md`](docs/superpowers/specs/2026-08-24-byovox-design.md).

License: MIT.

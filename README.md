# byovox

[![ci](https://img.shields.io/github/actions/workflow/status/BoJl4apa/byovox/ci.yml?branch=main&label=ci)](https://github.com/BoJl4apa/byovox/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/BoJl4apa/byovox)](https://github.com/BoJl4apa/byovox/releases)
[![license](https://img.shields.io/github/license/BoJl4apa/byovox)](LICENSE)

**Hold a key. Talk. Let go. Your words get typed into whatever window you're in.**

The speech model runs on your machine, not someone else's cloud. Windows only, for now.

## Get going in 3 steps

### 1. Install a speech model

Easiest path — [Ollama](https://ollama.com/download):

```sh
ollama pull ZimaBlueAI/whisper-large-v3   # hears you
ollama pull llama3.2                      # cleans up the text (optional, but nice)
```

Already running [whisper.cpp](https://github.com/ggml-org/whisper.cpp)? That works too:

```sh
whisper-server -m ggml-large-v3-turbo.bin --host 127.0.0.1 --port 8770 -l auto
```

### 2. Install byovox

Download the Windows zip from [Releases](https://github.com/BoJl4apa/byovox/releases) and unzip
it — keep both `.exe` files together. Windows warns on first run (More info → Run anyway),
because the binaries aren't code-signed.

Or build it with Rust 1.88+:

```sh
cargo install --git https://github.com/BoJl4apa/byovox --locked
```

### 3. Set it up, then talk

```sh
byovox setup
```

It finds Ollama on your machine, lists your models, and asks which one to use. Press Enter to
take the default. Stop halfway and your answers are still saved.

```sh
byovox
```

**Hold Right Ctrl, speak, release.** The text appears where your cursor is. Escape while holding
throws it away.

## When something's off

```sh
byovox check
```

It tests every part — mic, server, hotkey — and names the broken one.

```
ok   config    C:\Users\you\AppData\Roaming\byovox\config\config.toml
ok   hotkey    ControlRight hold, cancel Escape
ok   mic       Microphone Array (48000 Hz, 2 ch, F32)  peak -21.7 dBFS over 0.9s
ok   stt       0.34s  "Thank you."
ok   polish    0.46s  "So this is a test."
all required stages passed
```

## Commands

| | |
|---|---|
| `byovox` | start it (tray icon + hotkey) |
| `byovox setup` | the wizard |
| `byovox check` | self-test |
| `byovox last` | print the last thing you dictated |
| `byovox quit` | stop it |
| `byovox autostart --enable` | start at login |

Everything else: `byovox --help`.

## Speak more than one language?

byovox picks the language from your **keyboard layout**. Switch layout, switch dictation
language — nothing to configure per app.

```toml
[language]
candidates = ["en", "ru"]   # narrow auto-detect to these
[language.by_layout]
he = "he"                   # Hebrew layout → dictate in Hebrew
```

`byovox config --init` writes the config file. Every option is explained in
[docs/config.example.toml](docs/config.example.toml).

## Things to know

- **Your speech server can type into your windows.** Whatever it returns becomes keystrokes, so
  point byovox at servers you run.
- **Nothing is recorded by default.** No key logging, no saved audio, no telemetry. Turn on
  `capture_log` yourself if you want copies.
- **No secrets in the config file.** It holds the *name* of an environment variable, never a
  token.

Full threat model: [SECURITY.md](SECURITY.md). Windows specifics:
[docs/platform-windows.md](docs/platform-windows.md). Why it exists:
[the design doc](docs/superpowers/specs/2026-08-24-byovox-design.md).

License: MIT.

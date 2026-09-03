# byovox

[![ci](https://img.shields.io/github/actions/workflow/status/BoJl4apa/byovox/ci.yml?branch=main&label=ci)](https://github.com/BoJl4apa/byovox/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/BoJl4apa/byovox)](https://github.com/BoJl4apa/byovox/releases)
[![license](https://img.shields.io/github/license/BoJl4apa/byovox)](LICENSE)

**Hold a key. Talk. Let go. Your words get typed into whatever window you're in.**

The speech model runs on your machine, not someone else's cloud. Windows only, for now.

## Get going in 4 steps

### 1. Install a speech server

byovox needs a Whisper server. Get [whisper.cpp](https://github.com/ggml-org/whisper.cpp),
download a model from
[huggingface.co/ggerganov/whisper.cpp](https://huggingface.co/ggerganov/whisper.cpp/tree/main),
and run:

```sh
whisper-server -m ggml-large-v3-turbo.bin --host 127.0.0.1 --port 8770 -l auto \
  --inference-path /v1/audio/transcriptions
```

Both flags matter. `--inference-path` puts the server where OpenAI-compatible clients look;
without it you get `HTTP 404`. `-l auto` stops it forcing English and translating everything
into it.

**Ollama can't do this part.** Whisper models in the Ollama registry don't load, so you get
`HTTP 500`. Ollama *is* great for step 2.

### 2. Optional: clean-up model

This turns "um so like the thing is broken" into "So the thing is broken." Any
`/v1/chat/completions` server works — [Ollama](https://ollama.com/download) is easiest:

```sh
ollama pull gemma4:e2b
```

**Start small.** This model runs on every dictation and fights whisper for the same CPU. If
your laptop struggles with a 4B model, a 7B one will make dictating slower than typing. Too
slow? Use one of Ollama's free hosted models instead — but your transcripts leave your machine,
so byovox makes you
[opt in by hand](docs/platform-windows.md#3-text-clean-up-optional).

### 3. Install byovox

Download the Windows zip from [Releases](https://github.com/BoJl4apa/byovox/releases) and unzip
it — keep both `.exe` files together. Windows warns on first run (More info → Run anyway),
because the binaries aren't code-signed.

Or build it with Rust 1.88+:

```sh
cargo install --git https://github.com/BoJl4apa/byovox --locked
```

### 4. Set it up, then talk

```sh
byovox setup
```

It asks for your Whisper server, then finds Ollama and offers your models for the clean-up
step. Press Enter to take a default. Stop halfway and your answers are still saved.

```sh
byovox
```

**Hold Right Ctrl, speak, release.** The text appears where your cursor is. Escape while holding
throws it away.

Want it running at login, or stuck on any of the above? The full Windows walkthrough — models,
ffmpeg, autostart — is in [docs/platform-windows.md](docs/platform-windows.md).

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
| `byovox hotkey` | show or change the push-to-talk key |
| `byovox check` | self-test |
| `byovox last` | print the last thing you dictated |
| `byovox quit` | stop it |
| `byovox autostart --enable` | start at login |

Everything else: `byovox --help`.

## Don't like Right Ctrl?

```sh
byovox hotkey                          # what's bound now
byovox hotkey --list                   # every key you can pick
byovox hotkey --set F13                # bind it
byovox hotkey --set ControlLeft+ShiftLeft+Z --mode toggle
```

It refuses a key another app already owns, so you find out now instead of wondering why
something else keeps opening. `--mode toggle` means press once to start, once to send, instead
of holding.

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

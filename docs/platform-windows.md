# Windows

What Windows does differently, and the two things about it that surprise people.

## The hotkey

byovox installs a low-level keyboard hook (`WH_KEYBOARD_LL`) rather than registering a hotkey
with the shell, which is why a **bare modifier** works as a push-to-talk key: `ControlRight`
(the default), `AltRight`, `CapsLock`, `ScrollLock`, `Pause`, `Insert`, `F13`–`F24`. Every
accepted `hotkey.key` name is listed in [`config.example.toml`](config.example.toml); anything
else is refused at startup and by `byovox check`, which prints the full list.

The same hook is what makes a chord — `hotkey.key = "ControlLeft+ShiftLeft+Z"` — work, and it
**swallows the trigger**: pressed on top of its modifiers the `Z` reaches byovox and stops
there, so the editor underneath neither types a `z` nor sees a Ctrl+Shift+Z of its own.
Pressed without them it types as usual. The names are **virtual keys**, which the active
layout assigns — not physical positions. A non-Latin layout (Cyrillic, Hebrew, Greek) keeps
the US assignments for the alphanumeric block, so `Z` stays the key a US layout labels Z even
while it types я or ז. Another *Latin* layout follows its own printed letters instead: on
AZERTY `A` is the key US labels Q, and on QWERTZ `Z` is the key US labels Y — under those, a
chord key moves with the layout.

A **disabled** daemon swallows nothing: the tray's Disable disarms the hook as well as the
pipeline, so the chord's trigger types as usual until Enable puts it back. A chord already
recording when Disable lands still finishes — its repeats and its release stay swallowed, or
the window would get a key-up whose key-down it never saw.

The modifiers themselves pass through untouched — an app left holding a Shift that never comes
up is worse than any hotkey — so with the trigger swallowed the app sees them go down and come
back up with no key pressed in between, and Windows acts on exactly that: Ctrl+Left Shift sets
left-to-right and Ctrl+Right Shift right-to-left in RTL-aware editors, and Ctrl+Shift or
Alt+Shift switches the keyboard layout where that hotkey is enabled. byovox therefore taps one
unassigned virtual key (`0xFF`) the instant a chord fires: applications ignore a key nothing is
bound to, while the shell sees a key pressed with those modifiers and leaves the text direction
and the layout alone.

The hook sees every key on the desktop, including synthesised ones, so byovox stamps the
keystrokes it sends itself with a marker and ignores those: the Ctrl+V the `paste` rung sends
cannot read as a hotkey press, and neither can that tap.

No elevation, no driver, no service — the hook lives in the daemon's message loop and dies
with it.

## Insertion, and elevated windows

Three rungs, tried in the order below while `inject.mode = "auto"`:

| Rung | How | Notes |
|---|---|---|
| `type` | `SendInput` with `KEYEVENTF_UNICODE` | script-agnostic — Hebrew, Cyrillic and emoji land without a matching layout |
| `paste` | clipboard, Ctrl+V, previous clipboard **text** restored | a clipboard holding an image is replaced and not restored |
| `clipboard-only` | clipboard, nothing else | you press Ctrl+V |

UIPI — the Windows rule that a process cannot reach across integrity levels — meets byovox
before insertion does. A low-level keyboard hook installed by a non-elevated process is not
shown the keystrokes going to an elevated window, so while Task Manager's search box, an admin
PowerShell or an installer has focus the hotkey does nothing at all: no recording starts, the
tray icon stays grey, and the log gets no line. That is UIPI working, not a fault.

Dictating into elevated windows therefore means running byovox itself elevated, and that is a
bad trade to take lightly: it puts a global keyboard hook and a live microphone inside an
administrator process. Whether byovox should offer it at all, and behind what, is open —
<https://github.com/BoJl4apa/byovox/issues>.

A dictation can still end up facing an elevated window without the hook: `byovox toggle`
starts one with no key press involved, and focus can move while you speak. Then the same rule
applies to insertion instead — `type` and `paste` log `inject rung failed`, `clipboard-only`
succeeds, and the done cue means *the text is on the clipboard, press Ctrl+V*.

No rung ever presses Enter: a newline in the text becomes a space before it is typed or
pasted, so a dictation into a chat box that sends on Enter stays one unsent message, and one
into a terminal stays one unexecuted line. The built-in cleanup prompt formats a spoken
enumeration as a numbered list on one line for the same reason.

## Language routing

The layout byovox reads is the **foreground window's**, taken from that window's own thread —
which is exactly what Windows' "let me use a different input method for each app window"
setting controls. Win+Space therefore reroutes only the window you are in, not the desktop.

Reading the layout is not the same as routing on it. A layout becomes an explicit STT language
only if it has an entry in `[language.by_layout]`, and the shipped default leaves that map
empty: without `he = "he"` in it, a Hebrew window dictates under `language.default` exactly
like an English one, and the log line says `lang=auto` either way. Map the layouts you dictate
in — see [`config.example.toml`](config.example.toml) — and a Hebrew editor and an English
terminal then route to different languages with nothing configured per app.

Route a language you are not speaking and whisper does not transcribe it, it **translates**:
Hebrew speech under `language_candidates = ["en", "ru"]` comes back as English, and English or
Russian speech in a window on the Hebrew layout comes back as Hebrew. That is the model doing
what it was asked, deterministically, and it is the reason to keep the layout matched to the
language you are speaking rather than to the language you are writing about.

## Microphone level

Windows "audio enhancements" (Settings → System → Sound → the input device → Audio
enhancements) can attenuate a microphone by as much as 30 dB, and laptop array mics often ship
with them on. The symptom is not silence: transcripts come back empty, or as one hallucinated
stock phrase.

`byovox check` records a second and prints the peak, e.g. `peak -27.3 dBFS over 0.8s`. Below
-40 dBFS the row says so outright. If a normal speaking voice reads that low, turn the
enhancements off for that device and run `check` again.

### Bluetooth headsets

Connect a Bluetooth headset and Windows makes it the default *recording* device. A headset
carries a microphone only in its hands-free profile, so opening a capture stream on it drags
the whole device out of A2DP (stereo, full bandwidth) into that profile — mono at 8–16 kHz —
for as long as the stream is open. With byovox that is every dictation: music in the
headphones jumps in pitch and volume on the press and jumps back on the release, and the
transcript is worse than the built-in array would have given.

The playback byovox does — the start, done, warning and error cues — is not the cause. An output stream
leaves A2DP alone; only a *capture* stream forces the switch.

Pin the microphone instead of unpairing the headset:

```toml
[capture]
device = "Microphone Array"
```

Any case-insensitive substring of the device's name will do, and `byovox check` prints the
names to choose from on its `inputs` row — they come from the audio host, so they are shorter
than the Sound control panel's (`Microphone Array`, `Headset`). A name that matches no device
is refused at startup with that same list, and `check` warns when the microphone it used is a
hands-free endpoint.

## Cue output

The start, done, warning and error cues play on whatever Windows has as the default output device, and
they follow it: byovox registers for endpoint notifications (`IMMNotificationClient`), so
switching the default in the Sound panel — or switching a Bluetooth headset off, which
switches it for you — re-binds the cues to the device that took over, with one INFO
`cue output re-opened after an audio device change` line per re-bind (a change to another
endpoint, or a shared-mode format change, re-binds too — to the same device, with the same
line). A disconnect fires a burst of notifications and the
default only settles after the burst, so the re-bind is taken half a second after the first of
them — late enough that the burst is over, and capped so that an endpoint flapping without
pause is still re-bound once per half-second rather than postponed for ever.

Four things re-bind the cues: the default moving, an endpoint being removed, an endpoint
changing state, and a change to the default output's **shared-mode format** (device Properties
→ Advanced → Default Format), which invalidates every open stream on that endpoint without
moving the default or taking it out of service. Anything else that can kill a stream — another
application seizing the endpoint in exclusive mode, say — reports nothing to a listener, so it
is not covered.

The re-bind lands between cues, never during one: the tone after the switch is the first to
play on the new device. Without it the cues would simply stop — a stream whose endpoint has
gone keeps accepting tones without complaining — for the rest of that daemon's life. If no
output device is reachable at all, cues go quiet with a single WARN and every other layer
carries on; the next cue retries.

The tray's **Audio cues** item silences them for the running daemon and closes the output
stream with them. `indicator.cue` in the config is the value the daemon starts at, and the
item does not write back to it.

## Setup, start to finish

Four steps. Only the first two are required — byovox needs a speech server and nothing else.

### 1. Whisper server

byovox posts to `{stt.base_url}/audio/transcriptions`, the OpenAI shape. whisper.cpp serves it
but **not at that path by default**, which is the single most common way this fails.

1. Download `whisper-bin-x64.zip` from
   [whisper.cpp releases](https://github.com/ggml-org/whisper.cpp/releases) and unzip it, e.g.
   to `C:\tools\whisper`. (This is **whisper.cpp**, not the unrelated "WhisperFlow" product.)
2. Download a model from
   [huggingface.co/ggerganov/whisper.cpp](https://huggingface.co/ggerganov/whisper.cpp/tree/main)
   and drop the `.bin` beside the executables:

   | model | size | notes |
   |---|---|---|
   | `ggml-base.bin` | ~150 MB | fast, rough — fine for proving the pipeline works |
   | `ggml-large-v3-turbo.bin` | ~1.6 GB | the useful one on a CPU-only laptop |

3. Run it:

   ```sh
   whisper-server -m ggml-large-v3-turbo.bin --host 127.0.0.1 --port 8770 -l auto ^
     --inference-path /v1/audio/transcriptions
   ```

`--inference-path` is what puts it on `/v1/audio/transcriptions`. Without it whisper.cpp
listens on `/inference` and byovox reports `stt HTTP 404`. `-l auto` stops the server forcing
English and silently translating everything into it.

Add `-t 8` on a machine with cores to spare; the default is 4. Check the startup banner for
`whisper_backend_init_gpu: no GPU found` — CPU-only transcription of a long sentence takes
seconds, not milliseconds.

`byovox` then wants `stt.base_url = "http://127.0.0.1:8770/v1"`.

### 2. byovox

```sh
byovox setup
byovox check
```

### 3. Text clean-up (optional)

Any `/v1/chat/completions` server. [Ollama](https://ollama.com/download) is the easy one:

```sh
ollama pull gemma4:e2b
```

**Ollama cannot do step 1.** Whisper models in its registry do not load — Ollama runs
llama.cpp, which has no Whisper support, and `/v1/audio/transcriptions` answers `HTTP 500`
with `unknown model architecture`. Clean-up only.

**Watch the model size.** This model is resident and runs on every dictation, so it competes
with whisper for the same CPU and RAM. On a laptop that struggles with a 4B model, a 7B or
larger one will make dictation slower than typing. Start small: `gemma4:e2b`, `llama3.2:3b`,
`qwen2.5:3b`. If clean-up takes longer than a few seconds, the model is too big for the
machine — polish failing or timing out is not fatal (byovox types the raw transcript instead),
but you paid the wait for nothing.

If nothing local is fast enough, Ollama's hosted models (`gemma4:31b-cloud` and friends) are
far better and cost nothing to try — **but your transcripts leave the machine.** `byovox setup`
lists them after the local ones, marked *hosted by ollama.com*, and writes one only after a
question whose default is no. Or say so deliberately in the config:

```toml
[polish]
base_url = "http://127.0.0.1:11434/v1"
model    = "gemma4:31b-cloud"          # proxied by Ollama to ollama.com
hosted   = true                        # the URL is loopback; the text still leaves the machine
```

`hosted` is what `byovox check` reads to print its `note hosted` row. The wizard sets it for
you on this pick; set it by hand here or the check stays silent about a stage that is not
local.

Only the transcript is sent, never audio, and only when polish is enabled. If that is not a
trade you want, keep a small local model or set `polish.enabled = false`.

### 4. ffmpeg (optional, and usually unnecessary)

**byovox does not need ffmpeg.** It sends 16 kHz mono WAV, which whisper.cpp reads directly.
You need it only if you pass `--convert` to `whisper-server` to feed it other audio formats
yourself — and if you pass `--convert` without it, the server exits at startup with
`ffmpeg is not found`.

If you do want it: download a Windows build from
[gyan.dev](https://www.gyan.dev/ffmpeg/builds/) or
[BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases), unzip it, and rename the
versioned folder to something stable such as `C:\tools\ffmpeg` so an upgrade does not change
the path.

**Do not add it to `PATH` with `setx PATH "%PATH%;C:\tools\ffmpeg\bin"`.** In `cmd`, `%PATH%`
is the *system* PATH and the *user* PATH already joined, and `setx` writes the result to the
user PATH — so every system entry gets copied into your user PATH, permanently duplicated and
frozen at today's value. `setx` also truncates anything past 1024 characters, which silently
destroys a long PATH.

Use the editor instead: **Win+R → `rundll32 sysdm.cpl,EditEnvironmentVariables`** → under
*User variables* select `Path` → *Edit* → *New* → `C:\tools\ffmpeg\bin`. Or from PowerShell,
which touches only the user PATH:

```powershell
$user = [Environment]::GetEnvironmentVariable('Path','User')
[Environment]::SetEnvironmentVariable('Path', "$user;C:\tools\ffmpeg\bin", 'User')
```

Either way, open a new terminal before `where ffmpeg` will find it — a running shell keeps the
PATH it started with.

## Starting everything at login

Two separate things have to come up, and byovox does not manage the server.

**byovox** has it built in:

```sh
byovox autostart --enable
```

**whisper-server** does not. A shortcut in the Startup folder shows a console window at every
logon; a one-line VBScript launcher starts it hidden. Save this as
`C:\tools\whisper\start-hidden.vbs`:

```vbscript
CreateObject("WScript.Shell").Run _
  """C:\tools\whisper\whisper-server.exe"" -m ""C:\tools\whisper\ggml-large-v3-turbo.bin"" " & _
  "--host 127.0.0.1 --port 8770 -l auto --inference-path /v1/audio/transcriptions", 0, False
```

The `0` is what hides the window. Then press **Win+R**, run `shell:startup`, and put a shortcut
to that `.vbs` in the folder that opens. Remove the shortcut to undo it.

The model takes a few seconds to load, so the server may not be ready for a dictation in the
first moments after logon. byovox does not care: it starts, and the first press once the server
is up works normally. `byovox check` is how you confirm both halves are running.

To watch it start, run the `whisper-server` command in a terminal by hand instead — the banner
ends with `whisper server listening at http://127.0.0.1:8770`.

## Autostart

`byovox autostart --enable` writes the quoted path of `byovox-daemon.exe` — the binary beside
the CLI that the tray actually runs in — to
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` under the value name `byovox`;
`--disable` deletes it. Per-user, no elevation, no installer. Move or rename the binaries and
the value has to be written again.

`byovox-daemon.exe` is a windows-subsystem binary: it has no console, so nothing flashes at
logon and closing the terminal that ran `byovox` cannot take the tray with it. `byovox run` is
the same daemon inside the console CLI, for watching one start.

## Where things live

| | |
|---|---|
| config | `%APPDATA%\byovox\config\config.toml` |
| logs (daily, seven kept) | `%LOCALAPPDATA%\byovox\data\logs` |
| capture log, when enabled | `%APPDATA%\byovox\data\capture` |

Transcript text is logged only at `logging.level = "debug"`; at `info` the log carries
timings, the language sent and the rung used, never what you said. The capture log is the
exception: with `capture_log.enabled = true` it stores every raw and polished transcript, by
design — that is what it is for.

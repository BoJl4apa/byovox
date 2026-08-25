# Windows

What Windows does differently, and the two things about it that surprise people.

## The hotkey

byovox installs a low-level keyboard hook (`WH_KEYBOARD_LL`) rather than registering a hotkey
with the shell, which is why a **bare modifier** works as a push-to-talk key: `ControlRight`
(the default), `AltRight`, `CapsLock`, `ScrollLock`, `Pause`, `Insert`, `F13`–`F24`. Every
accepted `hotkey.key` name is listed in [`config.example.toml`](config.example.toml); anything
else is refused at startup and by `byovox check`, which prints the full list.

The hook sees every key on the desktop, including synthesised ones, so byovox stamps the
keystrokes it sends itself with a marker and ignores those: the Ctrl+V the `paste` rung sends
cannot read as a hotkey press.

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

One thing to know about `type`: a newline in the text is typed as Enter, because that is what
typing a newline means. The built-in cleanup prompt formats a spoken enumeration as a list,
one item per line — so dictating a list into a chat box that sends on Enter sends the first
item and types the rest into the next messages. Use `inject.mode = "paste"` where that
matters; `paste` puts the whole text in at once.

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

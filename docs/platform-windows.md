# Windows

What Windows does differently, and the two things about it that surprise people.

## The hotkey

byovox installs a low-level keyboard hook (`WH_KEYBOARD_LL`) rather than registering a hotkey
with the shell, which is why a **bare modifier** works as a push-to-talk key: `ControlRight`
(the default), `AltRight`, `CapsLock`, `ScrollLock`, `Pause`, `Insert`, `F13`–`F24`. The
accepted `hotkey.key` names are listed in [`config.example.toml`](config.example.toml).

The hook sees every key on the desktop, including synthesised ones, so byovox stamps the
keystrokes it sends itself with a marker and ignores those: dictating text that contains the
hotkey's own character cannot retrigger a recording.

No elevation, no driver, no service — the hook lives in the daemon's message loop and dies
with it.

## Insertion, and elevated windows

Three rungs, tried in the order below while `inject.mode = "auto"`:

| Rung | How | Notes |
|---|---|---|
| `type` | `SendInput` with `KEYEVENTF_UNICODE` | script-agnostic — Hebrew, Cyrillic and emoji land without a matching layout |
| `paste` | clipboard, Ctrl+V, previous clipboard **text** restored | a clipboard holding an image is replaced and not restored |
| `clipboard-only` | clipboard, nothing else | you press Ctrl+V |

UIPI — the Windows rule that a process cannot send input to a window running at a higher
integrity level — blocks `type` and `paste` into anything elevated: Task Manager's search box,
an admin PowerShell, most installers. That is expected, not a fault. Both rungs log
`inject rung failed`, `clipboard-only` succeeds, and the done cue then means *the text is on
the clipboard — press Ctrl+V*.

Running byovox itself elevated would lift the restriction and is a bad trade: it puts a global
keyboard hook and a live microphone inside an administrator process.

## Language routing

The layout byovox reads is the **foreground window's**, taken from that window's own thread —
which is exactly what Windows' "let me use a different input method for each app window"
setting controls. Win+Space therefore reroutes only the window you are in, so a Hebrew editor
and an English terminal dictate in different languages with nothing configured per app.

## Microphone level

Windows "audio enhancements" (Settings → System → Sound → the input device → Audio
enhancements) can attenuate a microphone by as much as 30 dB, and laptop array mics often ship
with them on. The symptom is not silence: transcripts come back empty, or as one hallucinated
stock phrase.

`byovox check` records a second and prints the peak, e.g. `peak -27.3 dBFS over 0.8s`. Below
-40 dBFS the row says so outright. If a normal speaking voice reads that low, turn the
enhancements off for that device and run `check` again.

## Autostart

`byovox autostart --enable` writes the quoted path of the current executable to
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` under the value name `byovox`;
`--disable` deletes it. Per-user, no elevation, no installer. Move or rename the binary and the
value has to be written again.

## Where things live

| | |
|---|---|
| config | `%APPDATA%\byovox\config\config.toml` |
| logs (daily, seven kept) | `%LOCALAPPDATA%\byovox\data\logs` |
| capture log, when enabled | `%APPDATA%\byovox\data\capture` |

Transcript text is logged only at `logging.level = "debug"`; at `info` the log carries
timings, the language sent and the rung used, never what you said.

# Manual checklist

Run this against the release build (`cargo build --release`) before a tag is pushed — that
binary is what the release workflow publishes, and nothing else here is exercised by CI.
Run `byovox check` first; every line below assumes it passed.

## Hotkey
- [ ] hold → release: text inserted; the pill showed recording then working
- [ ] tap (< 250 ms): nothing happens, no request in the log
- [ ] hold, then Escape: recording discarded, no request
- [ ] hold 3 s: exactly one `Pressed` (no auto-repeat), one dictation
- [ ] `mode = "toggle"`: press starts, press again sends
- [ ] `byovox toggle` twice from a terminal: one dictation

With `key = "ControlLeft+ShiftLeft+Z"`, into Notepad:

- [ ] hold the chord and speak: one dictation, and **nothing** typed into the window — no `z`,
      and a Hebrew paragraph keeps its direction (a bare Ctrl+Left Shift flips it)
- [ ] plain `Z`, no modifiers: types a `z`, no recording starts
- [ ] hold the chord, then let a modifier go first: recording ends and the text is inserted,
      and no `z` leaks when the trigger finally comes up

## Language routing

With `[language.by_layout] he = "he"` configured (see
[`docs/config.example.toml`](config.example.toml)) — the shipped default leaves that map empty,
and an unmapped layout routes to `language.default`, so every row below would log `auto`.

- [ ] Notepad with an English layout: log line says `lang=auto`
- [ ] switch that window to Hebrew (Win+Space): log line says `lang=he`, Hebrew comes back
- [ ] a *different* window on Hebrew while Notepad stays English: dictating into Notepad still logs `auto` (per-window layout)

## Insertion
- [ ] Notepad, a browser text field, Windows Terminal, VS Code: text lands with all scripts intact
- [ ] an elevated window (Task Manager's search, or an admin PowerShell): the hotkey is ignored while it has focus — no recording starts, the tray icon stays grey, no line in the log
- [ ] `inject.mode = "paste"`: clipboard content from before the dictation is restored afterwards
- [ ] polish endpoint down (wrong port): raw transcript inserted, error cue, WARN in log
- [ ] `inject.mode = "type"`, focus a chat box that sends on Enter, dictate "first foo, second bar": one *unsent* message reading `1. foo 2. bar` — nothing is submitted, no stray leading or trailing space
- [ ] dictate a plain sentence: the text lands with no final period; dictate a question: the `?` stays; with `inject.strip_terminal_period = false` the period is typed
- [ ] with `[stt.by_language.he]` configured and a Hebrew layout: `byovox check` shows an `ok stt[he]` row; dictate "אני עוד רבע שעה אצלו" → `רבע שעה` (the default lane types `רבשה`); dictate one word ("תודה") → it still lands, and the log shows `stt lane returned nothing; retrying the default endpoint`
- [ ] with a glossary in `stt.prompt`: dictate "שמנו את Tailscale מאחורי אותו proxy" under a Hebrew layout → `Tailscale` lands in Latin; dictate "ראיתי אריה בגן החיות" → `אריה` stays Hebrew

## Indicator
- [ ] first press after the daemon starts: the recording cue is audible (the output device is opened at start, not by that press)
- [ ] dictate a quiet mumble from across the room: the text still lands, the completion cue is the lower warning tone (not the short done blip), and the log has a `stt.no_speech_warn` WARN line with the probability
- [ ] tray icon: a mic glyph — grey idle, red recording, amber working, magenta for ~3 s on error; Disable shows the red ✕ over the grey mic
- [ ] tray menu: Disable stops the hotkey; Toggle mode takes its check mark and the hotkey then starts/stops on separate presses; Show last transcript shows a dialog; Open config / Open logs open Explorer; Run check opens a console that shows every row and waits for Enter; Quit exits
- [ ] with a chord hotkey and Disable active: pressing the chord types its trigger into the window (nothing is swallowed); Enable → the chord dictates again
- [ ] Run check under `byovox run`: the rows land in *that* terminal, interleaved with the live log, and its "press Enter" competes with the shell for stdin — expected; run the menu item against the background daemon
- [ ] tray menu, Audio cues: clear it and the next dictation is silent; check it again and the cue after that is audible (not the one straight after the click — the click is what opens the device)
- [ ] the pill never gets a taskbar button, on the first show and after a hide/re-show
- [ ] `indicator.pill = false` and `cue = false` each remove exactly that layer — and with `cue = false`, checking Audio cues from the tray brings them back for this daemon without editing the file
- [ ] with cues audible on a Bluetooth headset, switch the headset off mid-session: one `cue output re-opened after an audio device change` line in the log, and the next dictation's cues play on the device that took over; switch it back on and they follow it back

## Operations
- [ ] second `byovox` → "already running"
- [ ] close the terminal that ran `byovox`: the tray survives; `byovox run` and close its terminal: the tray exits
- [ ] `byovox status` / `byovox last` reflect the last dictation
- [ ] `capture_log.enabled = true`: WAV + JSONL row appear per dictation; transcript text absent from the INFO log
- [ ] `byovox autostart --enable`, sign out/in: daemon running; `--disable` removes it
- [ ] unknown key in config: exit 2 naming the key, from the bare `byovox` as well as from `byovox run` — and no daemon left behind
- [ ] `byovox setup` on a fresh config: probes pass, file written with comments intact, `check` passes
- [ ] `python bench/polish_bench.py` (`python3` on Linux and macOS): the `trap` and `cleanup` strata pass on every run; `punctuation` fails until #28 lands and `injection` until the prompt stops obeying an imperative dictation, each naming its items — a rule change is scored here (`--candidate <file>`, `--prompt-file <file>` for a copy), never eyeballed
- [ ] with a Bluetooth headset connected as the default recording device and `capture.device = "Microphone Array"`: `check`'s `mic` row names the array at 48 kHz, and a dictation leaves music in the headphones untouched (no pitch jump on press or release); a name that matches nothing → exit 2 listing the devices

## Last run

2026-08-25, Windows, release build. Rows an unattended run can exercise were run; the rest
need a human at the keyboard. The boxes above stay unticked — they are the template for each
release run.

Exercised by automation:

- Operations — second `byovox` → "already running"
- Operations — `byovox status` / `byovox last`, both before any dictation and after one
- Operations — `capture_log.enabled = true`: WAV + JSONL row, no transcript text in the INFO log
- Operations — unknown key in config: exit 2 naming the key
- Hotkey — `byovox toggle` twice from a terminal: one dictation
- Insertion — polish endpoint down (wrong port): raw transcript inserted, WARN in log
- `byovox check` against the real config, and `byovox config` / `config --init`

Left for the maintainer: everything that needs a physical key press (the rest of Hotkey), all
of Language routing, the three remaining Insertion rows, all of Indicator, and
`byovox autostart --enable` with a sign-out/in. The pill and the tray icon have to be looked
at, and layout switching has to happen on a real focused window.

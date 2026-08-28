# Security policy

byovox installs a global keyboard hook, opens your microphone, sends audio to a server you
configure, and types that server's answer into whatever window has focus. That is a large
amount of trust to hand a small program. This file says exactly where the boundaries are.

## Reporting a vulnerability

Report privately through GitHub's **private vulnerability reporting**: the *Security* tab of
this repository → *Report a vulnerability*. It is enabled under
*Settings → Code security → Private vulnerability reporting*; if the tab shows no such
option, open an issue asking for a contact address rather than posting details publicly.

Please include the byovox version (`byovox --version`), your OS, and the smallest steps that
reproduce it. There is no bounty. Expect a first response within a week.

Do not report the items under [Accepted by design](#accepted-by-design) — they are known and
documented. A concrete way to *break* one of those boundaries is a vulnerability and is very
welcome.

## Supported versions

`main` only, until a 1.0 release exists. Fixes land on `main`; there are no backports and no
maintained release branches. Build from source or use the most recent tag.

## Threat model

The user running byovox is trusted. Everything below is written from the point of view of an
attacker who is *not* that user, except where a row says otherwise.

| Boundary | What byovox does | In scope | Out of scope |
|---|---|---|---|
| **Keyboard hook** | `WH_KEYBOARD_LL` sees every keystroke in the session. Each event's virtual key is compared against the configured hotkey, chord members and cancel key; nothing else is read from it. No key is logged, stored, buffered or transmitted at any level — there is no `trace!` call in the tree, and the hook module logs only hook-installation failures. Only a chord's trigger is swallowed; every other key reaches the focused window. | A key event reaching a log, a file, the network, or `byovox last`. Swallowing a key the hotkey does not claim. | That the process *can* see keystrokes: that is what a global hotkey is. Anything with the same user account can install its own hook. |
| **STT / polish endpoint** | The transcript the server returns is **typed into the focused window** as synthetic keystrokes, or pasted. One sanitising pass runs first, on the text about to be injected, so every rung is covered: it turns every `\n` into a space, removes all other control characters (C0, DEL and C1 — tabs, carriage returns, terminal escapes) and the bidi overrides and isolates (U+202A–U+202E, U+2066–U+2069), and logs a WARN with the *count* dropped, never the text. Bidi marks and joiners (RLM/LRM, ZWJ/ZWNJ) are kept — Hebrew, Arabic and emoji are made of them. A reply left empty by that pass ends as an empty dictation. No rung ever presses Enter: a newline the server sends cannot submit a chat message or a shell line. A transcript longer than `inject.max_chars` (default 20 000) is **not typed at all** — it is held whole for `byovox last`, never truncated. | A byovox bug that types something the server did not send; a control or bidi-override character reaching the keyboard; the endpoint reaching beyond the focused window (running a command itself, writing files). | That a malicious endpoint can type arbitrary **text** into the focused window. Sanitising removes the keystrokes nobody speaks; it cannot tell a harmful sentence from a harmless one. **You are trusting that server as much as you trust your own keyboard.** Point byovox only at servers you run or trust. |
| **Secrets** | The bearer token comes from an environment variable, or a `NAME=VALUE` line in a key file. It is never logged, never put in an error string, and never printed by `byovox config` — the config file holds only the variable's *name* and the file's *path*. The HTTP client structs derive no `Debug`, so the token cannot reach a log through `{:?}`. | The token appearing in a log file, console output, a crash message, or a capture-log row. | The key file's permissions, which are whatever you set — byovox neither checks nor changes them, and the example config tells you to keep the file readable by you alone. Another process running as you can read your environment. |
| **Local IPC** | A per-user named pipe (`byovox-<user>.sock`) serving `status`, `last`, `toggle`, `quit`. It is created with the default named-pipe security descriptor, which grants full access to SYSTEM, Administrators and the creating user, and **read only** to Everyone and Anonymous. Every command must write a request line before the daemon answers, and writing requires an access right Everyone does not have — so a lower-integrity process (a sandboxed browser renderer) can neither open the microphone with `toggle` nor read a transcript with `last`. Remote clients are rejected outright. | A non-owner process sending any command, or reading a transcript. Reaching the socket over the network. | Processes running as the same user — they own the session, can read byovox's memory and can install their own hook. Denial of service by occupying the pipe's read handles or by claiming its name before byovox starts. |
| **Capture log** | Off by default. When `capture_log.enabled = true`, every dictation writes a **WAV of your voice** and a JSONL row containing the **raw and polished transcript in plain text** (the raw text is what the endpoint sent, with whisper's segment line breaks joined on a space, before sanitising) to `%APPDATA%\byovox\data\capture`. Files inherit the directory's permissions. Captures older than `capture_log.keep_days` (default 30) are deleted at startup and after each dictation; `0` keeps them for ever. Pruning is driven by the JSONL index and never by scanning the directory: a WAV is deleted only because an expired row names it, and only if that name also matches the exact form byovox writes. A file no row claims is never touched, whatever it is called. | byovox writing capture files when the setting is off, somewhere other than the configured directory, or keeping them past `keep_days`. Pruning deleting or corrupting anything byovox did not create. | The files themselves being readable by you and by anything running as you. Unbounded growth if you set `keep_days = 0` — that is the setting's meaning. |
| **Log file** | `%LOCALAPPDATA%\byovox\data\logs`, daily rotation, 7 files kept. At the default `info` level no transcript is written. At `debug`, the raw and polished transcripts are written to the log, along with up to 200 characters of any error response body the server returned. | A transcript reaching the log at `info` or above, or reaching the console or the tray. | What `debug` records: it is a diagnostic level you opt into. |
| **Clipboard** | The `paste` rung puts the transcript on the clipboard, sends Ctrl+V, and restores the previous **text** ~150 ms later. A clipboard holding an image or files is replaced and not restored. `clipboard-only` leaves the transcript on the clipboard indefinitely. | byovox failing to restore text it saved. | Clipboard managers and Windows clipboard history (Win+V) capturing the transcript, and **Windows Cloud Clipboard syncing it to your Microsoft account and other devices** if you have that on. Any process can read the clipboard during the paste window. |
| **Network** | Over plain HTTP your **audio, your transcript and your bearer token all cross the network in clear text**, and anyone on the path can replace the response with text byovox will type. `http://` to a non-loopback host is still allowed, but it is now called out: `byovox check` prints a `warn network` row per affected endpoint, and the daemon logs one WARN at startup. Loopback is quiet. HTTPS is verified normally against a bundled Mozilla root store; there is no switch to disable verification. | A missing or weakened certificate check on an `https://` endpoint; a cleartext endpoint that is *not* reported. | Using `http://` after being told — a reasonable choice to `localhost` or across a WireGuard/Tailscale link, a bad one across anything else. `check` still exits 0: this is a warning, not a failure. A privately-signed certificate will not verify, because the roots are the bundled Mozilla set, not the Windows store. |
| **Autostart** | `byovox autostart --enable` writes one value to `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` holding an absolute path to `byovox-daemon`, plus any `--config` you gave, absolutised. | byovox writing outside HKCU, or registering a relative path that would resolve elsewhere at logon. | That the registered binary sits somewhere you can write. Anything able to overwrite it is already running as you, and could add its own Run value instead. |
| **Injection marker** | Events byovox synthesises carry a public constant in `dwExtraInfo`; the hook ignores events carrying it, so byovox does not react to its own typing. | byovox failing to stamp an event it sends, which would make it react to itself. | Another process stamping the same value to hide its synthetic keys from byovox's hotkey. It suppresses a hotkey, gains nothing, and any process that can call `SendInput` as you can already type anywhere. The constant is not a secret. |
| **Supply chain** | `Cargo.lock` is committed and CI builds `--locked`, including the audit tools' own installs. `cargo audit` and `cargo deny check` run on every push; a vulnerability fails the build. A separate job builds against the declared MSRV, so `rust-version` is a checked promise rather than a claim. Every tag is built by a release workflow that publishes **a SHA-256 checksum beside the archive and a GitHub build-provenance attestation over the archive and over each binary inside it** — `sha256sum -c SHA256SUMS` and `gh attestation verify <zip> --owner BoJl4apa` tie a download to that workflow and to the commit it ran on. Release binaries are **not code-signed**. | A dependency vulnerability that CI should have caught; a build that does not match `Cargo.lock`; a published artefact whose checksum or attestation does not match what the workflow built. | SmartScreen warning on first run — the expected result of an unsigned binary; `cargo install` builds on your own machine and avoids it. Authenticode signing needs a paid certificate and is not committed to. |

### Accepted by design

These are properties of what byovox is, not bugs. They will not change:

- The configured endpoint can type arbitrary text into your focused window. Sanitising removes
  the keystrokes nobody speaks (and flattens newlines to spaces, so nothing is ever submitted),
  not the sentences you would not want typed.
- The process can see every keystroke while it runs, because that is how a global hotkey works.
- Anything running under your user account can reach the microphone, the IPC socket, the
  clipboard, the capture log and byovox's own memory. byovox draws no boundary against itself.

### Known limitations, accepted

Real, understood, and not being fixed — the cost of closing them exceeds what they buy:

- **The IPC socket can be denied, not driven.** Anything on the machine may open the pipe
  read-only and hold connection slots (255), or claim the name `byovox-<user>.sock` before
  byovox starts, which makes byovox refuse to start or lets the squatter answer the CLI. No
  transcript is exposed either way. Restricting the descriptor would close the first and not
  the second, and buys nothing against same-user processes, which dominate this boundary.
- **`logging.level = "debug"` writes transcripts to the log**, along with up to 200
  characters of any error response body the server returned. That is what the level is for;
  it is off by default and the log keeps 7 days.
- **`INJECT_MARKER` is a public constant.** Another process can stamp it to hide its own
  synthetic keys from byovox's hotkey. That suppresses a hotkey and gains nothing, and any
  process able to call `SendInput` as you can already type anywhere. Hiding the constant
  would be obscurity in a binary anyone can disassemble.

## Hardening checklist

If any of the above matters to you:

- Point `stt.base_url` and `polish.base_url` at `localhost`, or reach a remote host over
  WireGuard/Tailscale, or use `https://` with a publicly-trusted certificate. `byovox check`
  tells you which of your endpoints are in clear.
- Leave `capture_log.enabled = false`. If you turn it on, keep `capture_log.keep_days` at a
  number you are comfortable with — `0` means for ever.
- Leave `logging.level = "info"`. `debug` writes your transcripts to disk.
- Prefer the `type` rung over `paste` if clipboard history or cloud clipboard sync is on.
- Review what your polish endpoint is: it sees every transcript before you do.

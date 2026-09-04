#!/usr/bin/env python3
"""Polish bench: score the daemon's cleanup prompt, or a candidate rule, on text items.

The polish stage is text -> text, so no audio is needed: each item is a raw transcript the
way whisper writes it and the text the cleanup should produce. The bench composes the exact
system prompt the daemon sends (`BUILT_IN_PROMPT` read out of src/polish.rs, or the config's
`polish.prompt_file`, plus the glossary rule `prompt_for` appends), posts each item the way
`PolishClient::polish` does, and scores the reply after the pipeline's own terminal-period
strip. Outputs are not reproducible run to run (measured 2026-09-02, even at temperature 0),
so every item runs `--runs` times and passes only when every run matches.

    python bench/polish_bench.py                      # the current prompt, bench/polish_items.jsonl
    python bench/polish_bench.py --candidate rule.txt # the same, with a rule appended to the base
    python bench/polish_bench.py --prompt-file p.txt  # a copy of the prompt living elsewhere (voxdroid)
    python bench/polish_bench.py --capture <dictations.jsonl>  # byovox's capture log as the baseline
    python bench/polish_bench.py --no-capitalize      # the prompt polish.capitalize_first_word=false sends
    python bench/polish_bench.py --self-test          # no network: extraction and scoring

Strata: `punctuation` (spoken names to marks, #28), `trap` (words that only sound like one),
`cleanup` (fillers, repetitions), `injection` (content shaped like an instruction to the model),
`capitalization` (the first word left as spoken, #37). The last one is scored only when the
first-word rule is off and the others only when it is on — their expected texts disagree about
the first word — so the flag, or `polish.capitalize_first_word`, picks which half runs. A
selection that lands on the empty half exits 2: a gate may not pass with nothing measured.
Scoring compares punctuation like any other character — never stripped or normalised away —
because a punctuation rule is what this bench exists for.
Exit status is 0 when every selected stratum passes, 1 otherwise, 2 on a setup error. The
bearer token is never printed. Item text is printed for failures; capture-log rows are
counted, never printed, unless --verbose.
"""

import argparse
import json
import os
import re
import sys
import time
import unicodedata

try:
    import tomllib  # 3.11+
except ImportError:  # pragma: no cover
    print("polish_bench needs Python 3.11 or newer (tomllib)", file=sys.stderr)
    sys.exit(2)
import urllib.error
import urllib.request
from pathlib import Path
from typing import NoReturn

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
# `injection` is content shaped like an instruction to the model; rule 7 says it is content.
STRATA = ("punctuation", "trap", "cleanup", "injection", "capitalization")

# The first-word rule (#37) splits the items into two sets that can never be scored in one
# run. `capitalization` expects a lowercase first word, which is right only with the rule off;
# every other stratum's expected text carries the leading capital the default produces, so it
# is not a valid expectation with the rule off either. So `--no-capitalize` scores exactly the
# `capitalization` stratum and a default run scores exactly the rest. Neither set is ever
# marked wrong for the prompt obeying the rule it was given.
CAPITALIZATION = "capitalization"


def setup_error(msg: str) -> NoReturn:
    """A setup error: named on stderr, exit status 2 — `sys.exit(str)` would exit 1, the
    status a failed stratum uses."""
    print(msg, file=sys.stderr)
    sys.exit(2)


# ---------------------------------------------------------------- the prompt, as the daemon sends it


def built_in_prompt(src: str, capitalize: bool = True) -> str:
    """`BUILT_IN_PROMPT` out of src/polish.rs, byte for byte — or with rule 1 swapped, exactly
    as `polish::built_in` does it when `polish.capitalize_first_word` is false (#37)."""
    m = re.search(r'pub const BUILT_IN_PROMPT: &str = r#"(.*?)"#;', src, re.S)
    if not m:
        setup_error("polish.rs: BUILT_IN_PROMPT not found where the bench expects it")
    base = m.group(1)
    if capitalize:
        return base
    old, new = rule_one(src, "CAPITALISE_FIRST_RULE"), rule_one(src, "LOWERCASE_FIRST_RULE")
    if old not in base:
        setup_error("polish.rs: CAPITALISE_FIRST_RULE is not the rule 1 BUILT_IN_PROMPT carries")
    return base.replace(old, new, 1)


def rule_one(src: str, name: str) -> str:
    """One of the two rule-1 constants. Both are raw Rust literals of the same shape as
    BUILT_IN_PROMPT, so the bench reads them the way it reads that one; it has to, because it
    mirrors `polish::built_in`."""
    m = re.search(name + r': &str =\s*r#"(.*?)"#;', src, re.S)
    if not m:
        setup_error("polish.rs: " + name + " is not where the bench expects it")
    return m.group(1)

def glossary_rule(src: str) -> str:
    """The format string `system_prompt` appends when a glossary is configured, with Rust
    escapes resolved: `{}` is the base, `{g}` the glossary."""
    m = re.search(r'format!\(\s*"(\{\}\\n\\n10\. .*?\\n\{g\})",', src, re.S)
    if not m:
        setup_error("polish.rs: the glossary rule format string is not where the bench expects it")
    return m.group(1).replace("\\n", "\n")


def compose(base: str, glossary: str, rule: str) -> str:
    """`polish::system_prompt`: the base alone, or the base plus the glossary rule."""
    g = glossary.strip()
    if not g:
        return base
    head, tail = rule.split("{}", 1)
    return head + base.rstrip() + tail.replace("{g}", g, 1)


def fold_glossary(stt: dict) -> str:
    """`polish::prompt_for`: the [stt] glossary and every lane's, trimmed, non-empty, joined."""
    # `SttConfig::by_language` is a BTreeMap: lanes fold in code order, not file order.
    parts = [stt.get("prompt", "")] + [
        lane.get("prompt", "") for _, lane in sorted(stt.get("by_language", {}).items())
    ]
    return "\n".join(p.strip() for p in parts if p.strip())


def expand_home(p: str) -> Path:
    return Path(os.path.expanduser(p))


def resolve_token(env_name: str, file: str) -> str | None:
    """`config::resolve_token`: the env var, else the first `NAME=VALUE` line naming it."""
    if not env_name:
        return None
    v = os.environ.get(env_name, "").strip()
    if v:
        return v
    if not file:
        return None
    try:
        text = expand_home(file).read_text(encoding="utf-8")
    except OSError:
        return None
    for line in text.splitlines():
        k, sep, val = line.partition("=")
        if sep and k.strip() == env_name:
            val = val.strip().strip('"').strip("'").strip()
            return val or None
    return None


def default_config() -> Path:
    if sys.platform == "win32":
        return Path(os.environ["APPDATA"]) / "byovox" / "config" / "config.toml"
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "byovox" / "config.toml"
    return Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "byovox" / "config.toml"


def system_prompt_for(
    cfg: dict, candidate: str | None, prompt_file: Path | None = None, capitalize: bool = True
) -> str:
    """`--prompt-file` wins over the config's `polish.prompt_file`, which wins over the
    built-in: the glossary rule is composed on top of whichever base, as the daemon does."""
    polish = cfg.get("polish", {})
    src = (REPO / "src" / "polish.rs").read_text(encoding="utf-8")
    if prompt_file:
        base = prompt_file.read_text(encoding="utf-8")
    elif polish.get("prompt_file"):
        base = expand_home(polish["prompt_file"]).read_text(encoding="utf-8")
    else:
        base = built_in_prompt(src, capitalize)
    if candidate:
        # Appended to the base, before the glossary rule, numbered as the candidate file
        # numbers it: the bench reproduces what the daemon would send, it does not renumber.
        base = base.rstrip() + "\n" + candidate.strip() + "\n"
    return compose(base, fold_glossary(cfg.get("stt", {})), glossary_rule(src))


# ---------------------------------------------------------------- the endpoint's own speed

# Healthy, this stack generates ~63 tok/s; the two degraded states measured on it sat at
# 1.7-2.6 (llama.cpp evicted to CPU, and the same model starved while both GPUs ran at 100%).
# A floor an order of magnitude clear of both separates them without judging ordinary jitter.
MIN_TOK_S = 20.0


def speed_verdict(tok_s: float | None, floor: float) -> str | None:
    """The refusal message for a too-slow endpoint, or `None` to go ahead. Pure, so the
    self-test covers the decision without a network."""
    if tok_s is None or tok_s >= floor:
        return None
    return (
        f"endpoint generates {tok_s:.1f} tok/s, below the {floor:.0f} tok/s floor. Scores from a "
        "degraded endpoint are not comparable with scores from a healthy one — a model evicted "
        "to CPU produces different text, not merely slower text. Fix the endpoint (on this "
        "stack: `docker restart dictate`, and check nothing else is saturating the GPU), or "
        "pass --allow-slow-endpoint to score anyway and label the result."
    )


def endpoint_speed(url: str, model: str, token: str | None, timeout: float) -> float | None:
    """Generation tok/s from one short completion. `None` when the endpoint reports no usage,
    which is not a failure — it just means this check cannot run here."""
    body = json.dumps(
        {
            "model": model,
            "messages": [
                {"role": "system", "content": "Answer with the words only, nothing else."},
                {"role": "user", "content": "Count from one to ten in words."},
            ],
            "temperature": 0,
            "max_tokens": 40,
        }
    ).encode("utf-8")
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    start = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            reply = json.load(resp)
    except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError, ValueError) as e:
        setup_error(f"endpoint preflight: {e}")
    elapsed = time.monotonic() - start
    tokens = (reply.get("usage") or {}).get("completion_tokens")
    if not isinstance(tokens, int) or tokens < 1 or elapsed <= 0:
        return None
    return tokens / elapsed


# ---------------------------------------------------------------- one request, as PolishClient sends it


def polish(url: str, model: str, token: str | None, system: str, raw: str, timeout: float) -> str:
    body = json.dumps(
        {
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": f"<transcription>{raw}</transcription>"},
            ],
            "temperature": 0.3,
            "max_tokens": 1024,
        }
    ).encode("utf-8")
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            reply = json.load(resp)
    except urllib.error.HTTPError as e:
        # The leading 200 characters of the body, as PolishClient reports them: a wrong URL,
        # a refused key or an unknown model name says so there. Never the token.
        prefix = " ".join(e.read(200).decode("utf-8", "replace").split())
        raise RuntimeError(f"polish HTTP {e.code}: {prefix}") from None
    except (urllib.error.URLError, TimeoutError) as e:
        raise RuntimeError(f"polish transport: {e}") from None
    except ValueError as e:
        raise RuntimeError(f"polish JSON: {e}") from None
    try:
        content = reply["choices"][0]["message"]["content"]
    except (LookupError, TypeError):
        raise RuntimeError("polish response has no content field") from None
    if not isinstance(content, str) or not content.strip():
        raise RuntimeError("polish returned empty content")
    return content.strip()


# ---------------------------------------------------------------- scoring


def forbidden(c: str) -> bool:
    """`pipeline::is_forbidden`: every control character (Unicode Cc) and the bidi overrides
    and isolates; the marks and joiners Hebrew and emoji need are content and stay."""
    return unicodedata.category(c) == "Cc" or "\u202a" <= c <= "\u202e" or "\u2066" <= c <= "\u2069"


def typed(text: str) -> str:
    """What the pipeline types, step for step (`sanitize` then the tail of `Pipeline::finish`):
    every newline becomes a space, forbidden characters go, the ends are trimmed, exactly one
    terminal period is popped (an ellipsis stays) and the space it may expose goes with it.
    Nothing else is normalised: a doubled space in the reply is a doubled space typed."""
    t = "".join(" " if c == "\n" else c for c in text if c == "\n" or not forbidden(c)).strip()
    if t.endswith(".") and not t.endswith(".."):
        t = t[:-1].rstrip()
    return t


def levenshtein(a: str, b: str) -> int:
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def edit_rate(out: str, expected: str) -> float:
    e = typed(expected)
    return levenshtein(typed(out), e) / max(1, len(e))


# ---------------------------------------------------------------- items


def load_items(path: Path, only: str | None, capitalize: bool = True) -> list[dict]:
    items = []
    seen = set()
    for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        it = json.loads(line)
        for key in ("id", "stratum", "raw", "expected"):
            if not it.get(key):
                setup_error(f"{path}:{n}: item without {key}")
        if it["stratum"] not in STRATA:
            setup_error(f"{path}:{n}: unknown stratum {it['stratum']!r}; known: {', '.join(STRATA)}")
        if it["id"] in seen:
            setup_error(f"{path}:{n}: duplicate id {it['id']!r}")
        seen.add(it["id"])
        # See CAPITALIZATION: the flag selects which set of expectations is in force.
        if (it["stratum"] == CAPITALIZATION) == capitalize:
            continue
        if only is None or it["stratum"] == only:
            items.append(it)
    return items


def load_capture(path: Path) -> list[dict]:
    rows = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        r = json.loads(line)
        if r.get("polished") and r.get("raw", "").strip():
            rows.append(r)
    return rows


# ---------------------------------------------------------------- the two runs

SUMMARY: dict[str, dict[str, tuple[int, int]]] = {}


def run_items(items: list[dict], ask, runs: int, label: str, verbose: bool) -> bool:
    print(f"\n== {label}: {len(items)} items x {runs} runs")
    by_stratum: dict[str, list[bool]] = {s: [] for s in STRATA}
    for it in items:
        outs = []
        for _ in range(runs):
            try:
                outs.append(ask(it["raw"]))
            except RuntimeError as e:
                outs.append(f"<{e}>")
        passes = sum(typed(o) == typed(it["expected"]) for o in outs)
        worst = max(edit_rate(o, it["expected"]) for o in outs)
        ok = passes == runs
        by_stratum[it["stratum"]].append(ok)
        mark = "ok  " if ok else "FAIL"
        print(f"  {mark} {it['stratum']:<11} {it['id']:<14} {passes}/{runs}  worst edit rate {worst:.2f}")
        if not ok or verbose:
            print(f"       raw:      {it['raw']}")
            print(f"       expected: {typed(it['expected'])}")
            for o in outs:
                if typed(o) != typed(it["expected"]) or verbose:
                    print(f"       got:      {typed(o)}")
    all_ok = True
    totals = {}
    for s, results in by_stratum.items():
        if not results:
            continue
        good = sum(results)
        totals[s] = (good, len(results))
        state = "pass" if good == len(results) else "FAIL"
        all_ok &= good == len(results)
        print(f"  {s:<11} {good}/{len(results)} items  {state}")
    print(f"  total       {sum(g for g, _ in totals.values())}/{len(items)} items  {'pass' if all_ok else 'FAIL'}")
    SUMMARY[label] = totals
    return all_ok


def run_capture(rows: list[dict], ask, runs: int, label: str, verbose: bool) -> None:
    """Capture-log rows have no expected text: the stored `polished` is the baseline. Reported:
    how often a fresh output agrees with what was typed that day, and how often it edits words
    of the raw transcript at all (word-level, punctuation and case aside)."""
    words = lambda t: re.findall(r"\w+", t.lower())  # noqa: E731
    print(f"\n== {label}: {len(rows)} capture rows x {runs} runs")
    agree = edits = total = 0
    for r in rows:
        for _ in range(runs):
            try:
                out = ask(r["raw"])
            except RuntimeError as e:
                out = f"<{e}>"
            total += 1
            agree += typed(out) == typed(r["polished"])
            edits += words(out) != words(r["raw"])
            if verbose and typed(out) != typed(r["polished"]):
                print(f"  ts={r.get('ts')} stored: {typed(r['polished'])}\n       fresh:  {typed(out)}")
    print(f"  agrees with the stored polish {agree}/{total}; edits words of the raw transcript {edits}/{total}")


# ---------------------------------------------------------------- self-test, no network


def self_test() -> None:
    src = (REPO / "src" / "polish.rs").read_text(encoding="utf-8")
    base = built_in_prompt(src)
    assert base.startswith("You turn a raw speech transcript"), base[:40]
    assert "\n9. " in base and "\n10. " not in base, "the built-in prompt no longer stops at 9"
    rule = glossary_rule(src)
    assert rule.startswith("{}\n\n10. Technical terms") and rule.endswith("\n{g}"), rule[:60]
    assert compose(base, "  ", rule) == base
    with_g = compose(base, " Glossary: X ", rule)
    assert with_g.startswith(base.rstrip() + "\n\n10. ") and with_g.endswith("\nGlossary: X"), with_g[-40:]
    assert fold_glossary({"prompt": " A ", "by_language": {"he": {"prompt": "B"}, "ru": {"prompt": ""}}}) == "A\nB"
    assert fold_glossary({}) == ""
    assert typed("  Hello, world. ") == "Hello, world"
    assert typed("Hello .") == "Hello", "the pop must not expose a trailing space"
    assert typed("Wait...") == "Wait..."
    assert typed("Really?") == "Really?"
    assert typed("1. a\n2. b\n") == "1. a 2. b"
    assert typed("a\tb\u202ec\u2066d") == "abcd", "control and bidi-override characters go, as sanitize drops them"
    assert typed("Hello,  world") == "Hello,  world", "a doubled space is typed as is"
    assert typed("\u05e9\u05dc\u05d5\u05dd\u200f") == "\u05e9\u05dc\u05d5\u05dd\u200f", "the bidi mark is content"
    assert compose("Base with {g} literal.", "G", rule) == "Base with {g} literal.\n\n" + rule[4:].replace("{g}", "G", 1)
    assert fold_glossary({"by_language": {"ru": {"prompt": "R"}, "he": {"prompt": "H"}}}) == "H\nR", "lanes fold in code order"
    assert levenshtein("kitten", "sitting") == 3 and levenshtein("", "ab") == 2
    assert edit_rate("Hello world.", "Hello world") == 0.0
    assert abs(edit_rate("Hello there", "Hello world") - 5 / 11) < 1e-9
    lowered = built_in_prompt(src, False)
    assert lowered != base and rule_one(src, "LOWERCASE_FIRST_RULE") in lowered, "rule 1 swaps"
    assert rule_one(src, "CAPITALISE_FIRST_RULE") not in lowered
    assert lowered.replace(rule_one(src, "LOWERCASE_FIRST_RULE"), rule_one(src, "CAPITALISE_FIRST_RULE"), 1) == base, "only rule 1 moved"

    on = load_items(HERE / "polish_items.jsonl", None)
    off = load_items(HERE / "polish_items.jsonl", None, capitalize=False)
    assert {it["stratum"] for it in on} == set(STRATA) - {CAPITALIZATION}, "the default scores the rest"
    assert {it["stratum"] for it in off} == {CAPITALIZATION}, "the flag scores its own stratum"
    assert not [it for it in on if it in off], "the two sets are disjoint"
    assert {it["stratum"] for it in on + off} == set(STRATA), "every stratum has items"
    assert speed_verdict(63.3, MIN_TOK_S) is None, "a healthy endpoint is not refused"
    assert speed_verdict(None, MIN_TOK_S) is None, "an endpoint that reports no usage is not refused"
    assert speed_verdict(MIN_TOK_S, MIN_TOK_S) is None, "the floor itself passes"
    for degraded in (2.57, 1.89):
        msg = speed_verdict(degraded, MIN_TOK_S)
        assert msg and "not comparable" in msg, f"{degraded} tok/s must be refused"
    items = on + off
    assert all(typed(it["expected"]) == it["expected"] for it in items), "expected text is written as typed"
    print(f"self-test ok: prompt extracted, {len(items)} items parsed")


# ---------------------------------------------------------------- main


def main() -> int:
    # Items are Hebrew and Cyrillic; a Windows console defaults to a code page that cannot
    # print them and would kill the run on the first such item.
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    # stderr too: `setup_error` writes there, and its messages carry the same em dashes and
    # item text stdout was reconfigured for.
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", type=Path, default=default_config(), help="byovox config.toml (default: the daemon's)")
    ap.add_argument("--items", type=Path, default=HERE / "polish_items.jsonl")
    ap.add_argument("--capture", type=Path, help="a byovox dictations.jsonl; its polished column is the baseline")
    ap.add_argument("--candidate", type=Path, help="a rule appended to the base prompt; scored beside the baseline")
    ap.add_argument("--prompt-file", type=Path, help="a replacement base prompt, e.g. voxdroid's copy; the glossary rule is still composed on top")
    ap.add_argument("--runs", type=int, default=3, help="passes per item; an item passes only when every run matches")
    ap.add_argument("--only", choices=STRATA)
    ap.add_argument("--show-prompt", action="store_true", help="print the composed system prompt(s) and exit")
    ap.add_argument("--verbose", action="store_true", help="print every output, and capture rows that disagree")
    ap.add_argument(
        "--no-capitalize",
        action="store_true",
        help="compose the prompt the daemon sends with polish.capitalize_first_word = false (#37); "
        "scores the `capitalization` stratum, which is skipped otherwise",
    )
    ap.add_argument(
        "--min-tok-s",
        type=float,
        default=MIN_TOK_S,
        help=f"refuse to score when the endpoint generates below this (default {MIN_TOK_S:.0f})",
    )
    ap.add_argument(
        "--allow-slow-endpoint",
        action="store_true",
        help="score anyway when the preflight says the endpoint is degraded",
    )
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()

    if a.self_test:
        self_test()
        return 0
    try:
        cfg = tomllib.loads(a.config.read_text(encoding="utf-8"))
    except OSError as e:
        print(f"config: {e}", file=sys.stderr)
        return 2
    polish_cfg = cfg.get("polish", {})
    if not polish_cfg.get("base_url") or not polish_cfg.get("model"):
        print("config: polish.base_url and polish.model are required", file=sys.stderr)
        return 2
    url = polish_cfg["base_url"].rstrip("/") + "/chat/completions"
    token = resolve_token(polish_cfg.get("api_key_env", ""), polish_cfg.get("api_key_file", ""))
    timeout = float(polish_cfg.get("timeout_s", 20))  # PolishConfig's default
    # The flag, or the config's own key, exactly as the daemon resolves it.
    capitalize = polish_cfg.get("capitalize_first_word", True) and not a.no_capitalize
    # A replacement prompt owns its own rule 1 and `polish::built_in` never rewrites it, so
    # the first-word rule cannot be swapped into one — the items would switch while the prompt
    # did not, scoring lowercase expectations against a prompt never told to lower anything.
    if not capitalize and (a.prompt_file or polish_cfg.get("prompt_file")):
        setup_error(
            "the first-word rule is off, but the base prompt is a file, and a prompt file is "
            "never rewritten (polish::built_in) — its own rule 1 decides the first word. "
            "Drop --no-capitalize / polish.capitalize_first_word, or drop the prompt file."
        )
    if a.runs < 1:
        print("--runs must be at least 1", file=sys.stderr)
        return 2
    try:
        candidate = a.candidate.read_text(encoding="utf-8") if a.candidate else None
        prompts = [("baseline", system_prompt_for(cfg, None, a.prompt_file, capitalize))]
        if candidate:
            prompts.append(
                ("candidate", system_prompt_for(cfg, candidate, a.prompt_file, capitalize))
            )
    except OSError as e:
        print(f"prompt: {e}", file=sys.stderr)
        return 2
    if a.show_prompt:
        for label, p in prompts:
            print(f"---- {label}\n{p}")
        return 0

    try:
        items = load_items(a.items, a.only, capitalize)
        if not items and not a.capture:
            # Silent green is the one thing a gate may never be. `--only` is applied after the
            # first-word partition, so five of the ten combinations select nothing at all, and
            # `run_items` is skipped entirely — the run used to print a header and exit 0.
            which = f"--only {a.only}" if a.only else "the items file"
            setup_error(
                f"{which} selects no items with capitalize_first_word={capitalize}. "
                f"The `{CAPITALIZATION}` stratum is scored only with the rule off, every other "
                "stratum only with it on; nothing was measured, so this is not a pass."
            )
        rows = load_capture(a.capture) if a.capture else []
    except OSError as e:
        print(f"items: {e}", file=sys.stderr)
        return 2
    print(f"polish {polish_cfg['model']} at {polish_cfg['base_url']}; config {a.config}")
    # Before anything is scored: is this endpoint the one the numbers will be compared against?
    # The bench used to be silent about it, and produced hours of confident numbers from a
    # model that had fallen back to CPU. The rate is printed on every run so a result carries
    # the backend it was taken on.
    tok_s = endpoint_speed(url, polish_cfg["model"], token, timeout)
    if tok_s is None:
        print("endpoint speed: not reported by this server; the preflight cannot run here")
    else:
        print(f"endpoint speed: {tok_s:.1f} tok/s generated")
        refusal = speed_verdict(tok_s, a.min_tok_s)
        if refusal and not a.allow_slow_endpoint:
            setup_error(refusal)
        if refusal:
            print("  --allow-slow-endpoint given; scoring a degraded endpoint anyway")
    verdict = True
    for label, system in prompts:
        ask = lambda raw, s=system: polish(url, polish_cfg["model"], token, s, raw, timeout)  # noqa: E731
        ok = run_items(items, ask, a.runs, label, a.verbose) if items else True
        if rows:
            run_capture(rows, ask, a.runs, label, a.verbose)
        # With a candidate the exit status is the candidate's; alone, the baseline's.
        if label == prompts[-1][0]:
            verdict = ok
    if len(prompts) == 2 and SUMMARY.get("baseline") and SUMMARY.get("candidate"):
        print("\n== candidate vs baseline")
        for s in STRATA:
            if s in SUMMARY["baseline"]:
                b, c = SUMMARY["baseline"][s][0], SUMMARY["candidate"][s][0]
                print(f"  {s:<11} {b} -> {c} of {SUMMARY['baseline'][s][1]}  {'+' if c > b else '-' if c < b else '='}{abs(c - b)}")
    return 0 if verdict else 1


if __name__ == "__main__":
    sys.exit(main())

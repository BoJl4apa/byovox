"""Reads the same config.toml byovox's dictation path reads, and nothing else.

Stdlib only (tomllib, Python 3.11+) — mirrors ../bench/polish_bench.py's conventions rather
than adding a toml dependency. `[webui]` itself is read here too, but server.py's CLI flags
(--host/--port/--data-dir) win over the file, since the daemon already resolved those before
spawning this process.
"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

try:
    import tomllib
except ImportError:  # pragma: no cover - guarded at server.py entrypoint too
    print("pywebui needs Python 3.11 or newer (for tomllib)", file=sys.stderr)
    sys.exit(2)


def setup_error(msg: str) -> None:
    print(f"pywebui config error: {msg}", file=sys.stderr)
    sys.exit(2)


def expand_home(p: str) -> Path:
    if p.startswith("~/") or p.startswith("~\\"):
        return Path.home() / p[2:]
    return Path(p)


def resolve_token(env_name: str, file: str) -> str | None:
    """Bearer token: the named env var, else `NAME=VALUE` in `file`. Mirrors
    `config::resolve_token` in src/config.rs so the two never disagree about a credential."""
    if not env_name:
        return None
    v = os.environ.get(env_name, "").strip()
    if v:
        return v
    if not file:
        return None
    path = expand_home(file)
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None
    for line in text.splitlines():
        if "=" not in line:
            continue
        k, _, raw = line.partition("=")
        if k.strip() != env_name:
            continue
        value = raw.strip().strip('"').strip("'").strip()
        return value or None
    return None


@dataclass
class SttLane:
    base_url: str = ""
    model: str = ""
    prompt: str = ""


@dataclass
class SttConfig:
    base_url: str = ""
    model: str = "whisper-1"
    api_key_env: str = ""
    api_key_file: str = ""
    prompt: str = ""
    timeout_s: int = 30
    language_candidates: list[str] = field(default_factory=list)
    by_language: dict[str, SttLane] = field(default_factory=dict)

    def token(self) -> str | None:
        return resolve_token(self.api_key_env, self.api_key_file)

    def glossary(self) -> str:
        """`stt.prompt` plus every lane's prompt, folded into one glossary line — the same
        set `polish::prompt_for` appends to the cleanup prompt in src/polish.rs."""
        terms = [self.prompt] + [lane.prompt for lane in self.by_language.values()]
        return " ".join(t for t in terms if t.strip())


@dataclass
class PolishConfig:
    enabled: bool = True
    base_url: str = ""
    model: str = ""
    api_key_env: str = ""
    api_key_file: str = ""
    capitalize_first_word: bool = True
    timeout_s: int = 20
    prompts: dict[str, str] = field(default_factory=dict)
    analysis_max_chars: int = 8000
    analysis_overlap_ratio: float = 0.1

    def token(self) -> str | None:
        return resolve_token(self.api_key_env, self.api_key_file)


@dataclass
class LanguageConfig:
    default: str = "auto"
    candidates: list[str] = field(default_factory=list)


@dataclass
class WebuiConfig:
    enabled: bool = False
    host: str = "0.0.0.0"
    port: int = 8787
    stt_base_url: str = ""
    silence_threshold_db: float = -35.0
    silence_min_s: float = 10.0
    audio_retention_days: int = 7
    ffmpeg_path: str = ""
    stt_timeout_s: int = 600
    llm_timeout_s: int = 300
    prompts: dict[str, str] = field(default_factory=dict)
    analysis_max_chars: int = 8000
    analysis_overlap_ratio: float = 0.1


@dataclass
class Config:
    stt: SttConfig = field(default_factory=SttConfig)
    language: LanguageConfig = field(default_factory=LanguageConfig)
    polish: PolishConfig = field(default_factory=PolishConfig)
    webui: WebuiConfig = field(default_factory=WebuiConfig)


def _stt(table: dict[str, Any]) -> SttConfig:
    lanes = {
        code: SttLane(**{k: v for k, v in lane.items() if k in ("base_url", "model", "prompt")})
        for code, lane in table.get("by_language", {}).items()
    }
    return SttConfig(
        base_url=table.get("base_url", ""),
        model=table.get("model", "whisper-1"),
        api_key_env=table.get("api_key_env", ""),
        api_key_file=table.get("api_key_file", ""),
        prompt=table.get("prompt", ""),
        timeout_s=table.get("timeout_s", 30),
        by_language=lanes,
    )


def _polish(table: dict[str, Any]) -> PolishConfig:
    return PolishConfig(
        enabled=table.get("enabled", True),
        base_url=table.get("base_url", ""),
        model=table.get("model", ""),
        api_key_env=table.get("api_key_env", ""),
        api_key_file=table.get("api_key_file", ""),
        capitalize_first_word=table.get("capitalize_first_word", True),
        timeout_s=table.get("timeout_s", 20),
    )


def _language(table: dict[str, Any]) -> LanguageConfig:
    return LanguageConfig(
        default=table.get("default", "auto"),
        candidates=table.get("candidates", []),
    )


def _webui(table: dict[str, Any]) -> WebuiConfig:
    return WebuiConfig(
        enabled=table.get("enabled", False),
        host=table.get("host", "0.0.0.0"),
        port=table.get("port", 8787),
        stt_base_url=table.get("stt_base_url", ""),
        silence_threshold_db=table.get("silence_threshold_db", -35.0),
        silence_min_s=table.get("silence_min_s", 10.0),
        audio_retention_days=table.get("audio_retention_days", 7),
        ffmpeg_path=table.get("ffmpeg_path", ""),
        stt_timeout_s=table.get("stt_timeout_s", 600),
        llm_timeout_s=table.get("llm_timeout_s", 300),
        prompts={key: value for key, value in table.get("prompts", {}).items() if isinstance(value, str)},
        analysis_max_chars=table.get("analysis_max_chars", 8000),
        analysis_overlap_ratio=table.get("analysis_overlap_ratio", 0.1),
    )


def load(path: Path) -> Config:
    """Missing file = every default, same as `config::load` on the Rust side — server.py's
    CLI flags are what actually matter for host/port/data-dir; this only supplies the STT
    and polish endpoints, which have no CLI equivalent."""
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return Config()
    try:
        raw = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        setup_error(f"parsing {path}: {e}")
    return Config(
        stt=_stt(raw.get("stt", {})),
        language=_language(raw.get("language", {})),
        polish=_polish(raw.get("polish", {})),
        webui=_webui(raw.get("webui", {})),
    )

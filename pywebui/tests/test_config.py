from pathlib import Path

from pywebui.config import load


def test_webui_prompt_overrides_load_from_toml(tmp_path: Path):
    path = tmp_path / "config.toml"
    path.write_text(
        """
[stt]
base_url = "http://127.0.0.1:8771/v1"

[webui]
[webui.prompts]
summary = "Return decisions only."
relationship = "Compare the two supplied notes."
""",
        encoding="utf-8",
    )

    cfg = load(path)

    assert cfg.webui.prompts == {
        "summary": "Return decisions only.",
        "relationship": "Compare the two supplied notes.",
    }

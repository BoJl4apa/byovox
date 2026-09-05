from pywebui import llm
from pywebui.quality import score_node


def test_capitalize_toggle_changes_rule_one():
    on = llm.full_transcript_polish_prompt(True, "")
    off = llm.full_transcript_polish_prompt(False, "")
    assert "Add punctuation and capitalisation" in on
    assert "without capitalising the first word" in off


def test_glossary_appended_when_present():
    prompt = llm.full_transcript_polish_prompt(True, "Acme, Kubernetes")
    assert "Acme, Kubernetes" in prompt
    assert "Glossary" in prompt


def test_no_glossary_rule_when_empty():
    prompt = llm.full_transcript_polish_prompt(True, "")
    assert "Glossary" not in prompt


def test_prompt_catalog_applies_known_overrides_only():
    catalog = llm.prompt_catalog(True, "", {"summary": "Return only decisions.", "unknown": "ignore"})
    assert catalog["summary"] == "Return only decisions."
    assert "unknown" not in catalog


def test_parse_numbered_round_trip():
    reply = "1: Hello there.\n2: How are you?\n"
    out = llm._parse_numbered(reply, 2)
    assert out == ["Hello there.", "How are you?"]


def test_parse_numbered_rejects_wrong_count():
    reply = "1: Hello there.\n"
    assert llm._parse_numbered(reply, 2) is None


def test_parse_numbered_rejects_missing_expected_label():
    reply = "0: Hello there.\n2: How are you?\n"
    assert llm._parse_numbered(reply, 2) is None


def test_extract_json_array_from_noisy_reply():
    reply = 'Sure, here it is:\n[{"title": "a", "start_line": 1, "end_line": 2}]\nThanks!'
    assert llm._extract_json_array(reply) == '[{"title": "a", "start_line": 1, "end_line": 2}]'


def test_windows_splits_on_char_budget():
    lines = ["x" * 100 for _ in range(10)]
    windows = llm._windows(lines, max_chars=250)
    assert sum(len(w) for w in windows) == 10
    assert all(sum(len(x) for x in w) <= 350 for w in windows)  # generous slack for boundary


def test_overlapping_windows_carry_context():
    windows = llm._overlapping_windows(["a" * 10 for _ in range(10)], max_chars=35, overlap_ratio=0.2)
    assert len(windows) > 1
    assert set(windows[0]) & set(windows[1])

    def test_topic_titles_fit_header_budget():
        title = llm._fit_title("A very long conversation title " * 5)
        assert len(title) <= 72
        assert title.endswith("...")


def test_node_scores_explain_completeness_and_quality():
    scores = score_node(
        "Planning",
        "We discussed the launch date and assigned the next steps to the team.",
        "The team agreed on a launch date and next steps.",
        12.5,
        28.0,
    )
    assert scores["completeness"] >= 90
    assert scores["quality"] >= 90
    assert scores["completeness_factors"]["timestamps"] == 10


def test_node_scores_penalize_empty_capture():
    scores = score_node("Untitled", "", "", None, None)
    assert scores["completeness"] == 15
    assert scores["quality"] == 0

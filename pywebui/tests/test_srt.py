from pywebui import noise, srt


def test_build_and_parse_round_trip():
    segments = [
        srt.Segment(start=0.0, end=1.5, text="Hello there."),
        srt.Segment(start=1.5, end=3.25, text="How are you?"),
    ]
    text = srt.build(segments)
    assert "00:00:00,000 --> 00:00:01,500" in text
    assert "00:00:01,500 --> 00:00:03,250" in text

    parsed = srt.parse(text)
    assert [s.text for s in parsed] == ["Hello there.", "How are you?"]
    assert parsed[0].start == 0.0
    assert parsed[1].end == 3.25


def test_empty_segments_are_dropped():
    segments = [srt.Segment(start=0.0, end=1.0, text="   ")]
    assert srt.build(segments) == ""


def test_to_plain_lines_has_hh_mm_ss_prefix():
    segments = [srt.Segment(start=61.0, end=62.0, text="one minute in")]
    text = srt.build(segments)
    lines = srt.to_plain_lines(text)
    assert lines == "[00:01:01] one minute in"


def test_repeated_short_phrases_are_preserved_as_noise():
    segments = [
        srt.Segment(start=1.0, end=2.0, text="okay sounds"),
        srt.Segment(start=4.0, end=5.0, text="actual note"),
        srt.Segment(start=7.0, end=8.0, text="okay sounds"),
        srt.Segment(start=10.0, end=11.0, text="okay sounds"),
    ]
    clean, flagged = noise.classify(segments)
    assert [segment.text for segment in clean] == ["actual note"]
    assert len(flagged) == 3
    assert flagged[0]["start"] == 1.0

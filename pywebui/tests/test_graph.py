from pathlib import Path

from pywebui import graph


def test_graph_links_lexically_related_nodes(tmp_path: Path):
    path = tmp_path / "graph.json"
    graph.update(path, {"id": "one", "title": "Launch plan", "summary": "release date", "archived": False})
    data = graph.update(path, {"id": "two", "title": "Launch review", "summary": "release date", "archived": False})
    assert len(data["edges"]) == 1
    assert data["edges"][0]["strength"] > 0


def test_graph_relation_callback_can_override_strength(tmp_path: Path):
    path = tmp_path / "graph.json"
    graph.update(path, {"id": "one", "title": "Launch plan", "summary": "release date", "archived": False})
    data = graph.update(
        path,
        {"id": "two", "title": "Launch review", "summary": "release date", "archived": False},
        relation=lambda current, previous: 0.91,
    )
    assert data["edges"][0]["strength"] == 0.91


def test_graph_keeps_lexical_score_when_relation_fails(tmp_path: Path):
    path = tmp_path / "graph.json"
    graph.update(path, {"id": "one", "title": "Launch plan", "summary": "release date", "archived": False})
    data = graph.update(
        path,
        {"id": "two", "title": "Launch review", "summary": "release date", "archived": False},
        relation=lambda current, previous: (_ for _ in ()).throw(RuntimeError("offline")),
    )
    assert data["edges"][0]["strength"] > 0

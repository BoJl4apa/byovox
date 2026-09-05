"""FastAPI app: two things, per the spec — upload a recording, download/browse past
transcripts. No authentication (see `[webui]` in config.example.toml): meant for a trusted
home LAN only.
"""

from __future__ import annotations

import dataclasses
import html
import json
from pathlib import Path

from fastapi import FastAPI, Form, Request, UploadFile
from fastapi.responses import HTMLResponse, JSONResponse, PlainTextResponse, RedirectResponse
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates

from . import audio, llm, pipeline, srt
from .config import Config
from .jobs import JobQueue
from .storage import Chunk, Storage

ALLOWED_SUFFIXES = {".m4a", ".mp3"}
HERE = Path(__file__).resolve().parent


def create_app(cfg: Config, data_dir: Path) -> FastAPI:
    storage = Storage(data_dir)
    jobs = JobQueue()
    templates = Jinja2Templates(directory=str(HERE / "templates"))
    app = FastAPI(title="byovox webui")
    static_dir = HERE / "static"
    if static_dir.is_dir():
        app.mount("/static", StaticFiles(directory=str(static_dir)), name="static")

    @app.get("/", response_class=HTMLResponse)
    def index(request: Request):
        recordings = []
        for rec in storage.list():
            meta = rec.read_metadata()
            recordings.append(meta)
        sort = request.query_params.get("sort", "date")
        view = request.query_params.get("view", "dates")
        if sort == "quality":
            recordings.sort(key=lambda meta: max((chunk.quality for chunk in meta.chunks), default=-1), reverse=True)
        elif sort == "completeness":
            recordings.sort(key=lambda meta: max((chunk.completeness for chunk in meta.chunks), default=-1), reverse=True)
        else:
            recordings.sort(key=lambda meta: meta.created_at, reverse=True)
        date_groups: dict[str, list] = {}
        for meta in recordings:
            date_groups.setdefault(meta.created_at[:10], []).append(meta)
        graph_path = storage.recordings_dir.parent / "graph.json"
        graph_data = json.loads(graph_path.read_text(encoding="utf-8")) if graph_path.exists() else {"nodes": [], "edges": []}
        return templates.TemplateResponse(
            "index.html", {
                "request": request,
                "recordings": recordings,
                "date_groups": date_groups,
                "view": view,
                "sort": sort,
                "graph_data": graph_data,
            }
        )

    @app.post("/upload")
    async def upload(file: UploadFile):
        suffix = Path(file.filename or "").suffix.lower()
        if suffix not in ALLOWED_SUFFIXES:
            return PlainTextResponse("only .m4a and .mp3 are accepted", status_code=400)
        rec = storage.create(file.filename or "recording")
        dest = rec.original_path(suffix)
        with dest.open("wb") as out:
            while chunk := await file.read(1024 * 1024):
                out.write(chunk)
        # Probe immediately when possible, but keep an unreadable upload for diagnosis and
        # retry. Removing it here would discard the only copy of an interrupted recording.
        try:
            audio.probe_duration(dest, cfg.webui.ffmpeg_path)
        except audio.FfmpegFailed as e:
            rec.set_status("error", error=str(e))
            return RedirectResponse("/", status_code=303)
        jobs.submit(lambda: pipeline.process(cfg, storage, rec))
        return RedirectResponse("/", status_code=303)

    @app.get("/recordings/{recording_id}", response_class=HTMLResponse)
    def recording_detail(request: Request, recording_id: str):
        rec = storage.get(recording_id)
        if rec is None:
            return PlainTextResponse("not found", status_code=404)
        meta = rec.read_metadata()
        sort = request.query_params.get("sort", "date")
        if sort == "quality":
            meta.chunks.sort(key=lambda chunk: chunk.quality, reverse=True)
        elif sort == "completeness":
            meta.chunks.sort(key=lambda chunk: chunk.completeness, reverse=True)
        else:
            meta.chunks.sort(key=lambda chunk: chunk.start)
        chunks = []
        refined = rec.read_refined()
        refinement_history = rec.read_refinement_history()
        for c in meta.chunks:
            text_path = rec.chunk_txt(c.index, c.slug)
            summary_path = rec.chunk_summary_txt(c.index, c.slug)
            chunks.append(
                {
                    "title": c.title,
                    "text": refined.get(f"chunk-text-{c.index}", text_path.read_text(encoding="utf-8"))
                    if text_path.exists()
                    else refined.get(f"chunk-text-{c.index}", ""),
                    "summary": refined.get(f"chunk-summary-{c.index}", summary_path.read_text(encoding="utf-8"))
                    if summary_path.exists()
                    else refined.get(f"chunk-summary-{c.index}", ""),
                    "index": c.index,
                    "text_history": refinement_history.get(f"chunk-text-{c.index}", []),
                    "summary_history": refinement_history.get(f"chunk-summary-{c.index}", []),
                    "start": c.start,
                    "end": c.end,
                    "start_seconds": c.start_seconds if c.start_seconds is not None else c.start,
                    "end_seconds": c.end_seconds if c.end_seconds is not None else c.end,
                    "completeness": c.completeness,
                    "quality": c.quality,
                    "completeness_factors": c.completeness_factors,
                    "quality_factors": c.quality_factors,
                    "archived": c.archived,
                    "parent_index": c.parent_index,
                    "version": c.version,
                }
            )
        transcript = ""
        if rec.final_srt.exists():
            transcript = srt.to_plain_lines(rec.final_srt.read_text(encoding="utf-8"))
        transcript = refined.get("transcript", transcript)
        noise_items = []
        if rec.noise_json.exists():
            noise_items = json.loads(rec.noise_json.read_text(encoding="utf-8"))
        summary = refined.get(
            "summary", rec.summary_txt.read_text(encoding="utf-8") if rec.summary_txt.exists() else ""
        )
        return templates.TemplateResponse(
            "recording.html",
            {
                "request": request,
                "meta": meta,
                "chunks": chunks,
                "transcript": transcript,
                "summary": summary,
                "summary_history": refinement_history.get("summary", []),
                "transcript_history": refinement_history.get("transcript", []),
                "sort": sort,
                "noise_items": noise_items,
                "has_raw_transcript": rec.raw_srt.exists(),
            },
        )

    @app.get("/recordings/{recording_id}/download")
    def download(recording_id: str):
        rec = storage.get(recording_id)
        if rec is None:
            return PlainTextResponse("not found", status_code=404)
        text = pipeline.combined_download_text(rec)
        return PlainTextResponse(
            text,
            media_type="text/plain; charset=utf-8",
            headers={
                "Content-Disposition": 'attachment; filename="transcript.txt"'
            },
        )

    @app.post("/recordings/{recording_id}/retry")
    def retry(recording_id: str):
        rec = storage.get(recording_id)
        if rec is None:
            return PlainTextResponse("not found", status_code=404)
        if rec.existing_original() is None:
            return PlainTextResponse(
                "original audio no longer available (past retention), cannot retry",
                status_code=409,
            )
        rec.reset_for_retry()
        jobs.submit(lambda: pipeline.process(cfg, storage, rec))
        return RedirectResponse(f"/recordings/{recording_id}", status_code=303)

    @app.post("/recordings/{recording_id}/recalculate")
    def recalculate(recording_id: str):
        rec = storage.get(recording_id)
        if rec is None:
            return PlainTextResponse("not found", status_code=404)
        if not rec.raw_srt.exists():
            return PlainTextResponse("recording has no completed transcription", status_code=409)
        rec.reset_for_recalculation()
        jobs.submit(lambda: pipeline.process(cfg, storage, rec))
        return RedirectResponse(f"/recordings/{recording_id}", status_code=303)

    @app.post("/recordings/{recording_id}/refine")
    def refine_result(
        recording_id: str,
        target: str = Form(...),
        text: str = Form(...),
        instruction: str = Form(...),
    ):
        rec = storage.get(recording_id)
        if rec is None:
            return PlainTextResponse("not found", status_code=404)
        allowed = {"summary", "transcript"}
        if target.startswith("chunk-summary-") or target.startswith("chunk-text-"):
            try:
                int(target.rsplit("-", 1)[1])
            except ValueError:
                return PlainTextResponse("invalid target", status_code=400)
            allowed.add(target)
        if target not in allowed:
            return PlainTextResponse("invalid target", status_code=400)
        try:
            revised = llm.refine(
                dataclasses.replace(cfg.polish, timeout_s=cfg.webui.llm_timeout_s),
                text,
                instruction,
                storage.interactions_log,
                recording_id,
            )
            rec.write_refined(target, revised)
            rec.add_refinement_history(target, instruction, text, revised)
        except RuntimeError as e:
            return JSONResponse({"error": str(e)}, status_code=502)
        return JSONResponse({"target": target, "text": revised})

    @app.post("/recordings/{recording_id}/nodes/{node_index}/archive")
    def archive_node(recording_id: str, node_index: int):
        rec = storage.get(recording_id)
        if rec is None:
            return PlainTextResponse("not found", status_code=404)
        meta = rec.read_metadata()
        node = next((chunk for chunk in meta.chunks if chunk.index == node_index), None)
        if node is None:
            return PlainTextResponse("node not found", status_code=404)
        node.archived = True
        rec.write_metadata(meta)
        _set_graph_node_archived(storage, f"{recording_id}:{node_index}", True)
        return RedirectResponse(f"/recordings/{recording_id}", status_code=303)

    @app.post("/recordings/{recording_id}/nodes/{node_index}/unarchive")
    def unarchive_node(recording_id: str, node_index: int):
        rec = storage.get(recording_id)
        if rec is None:
            return PlainTextResponse("not found", status_code=404)
        meta = rec.read_metadata()
        node = next((chunk for chunk in meta.chunks if chunk.index == node_index), None)
        if node is None:
            return PlainTextResponse("node not found", status_code=404)
        node.archived = False
        rec.write_metadata(meta)
        _set_graph_node_archived(storage, f"{recording_id}:{node_index}", False)
        return RedirectResponse(f"/recordings/{recording_id}", status_code=303)

    @app.post("/recordings/{recording_id}/nodes/{node_index}/version")
    def version_node(recording_id: str, node_index: int):
        rec = storage.get(recording_id)
        if rec is None:
            return PlainTextResponse("not found", status_code=404)
        meta = rec.read_metadata()
        source = next((chunk for chunk in meta.chunks if chunk.index == node_index), None)
        if source is None:
            return PlainTextResponse("node not found", status_code=404)
        new_index = max((chunk.index for chunk in meta.chunks), default=0) + 1
        slug = f"{source.slug}-v{source.version + 1}"
        text = rec.chunk_txt(source.index, source.slug).read_text(encoding="utf-8")
        summary = rec.chunk_summary_txt(source.index, source.slug).read_text(encoding="utf-8")
        rec.chunk_txt(new_index, slug).write_text(text, encoding="utf-8")
        rec.chunk_summary_txt(new_index, slug).write_text(summary, encoding="utf-8")
        meta.chunks.append(
            Chunk(
                index=new_index, title=source.title, start=source.start, end=source.end,
                slug=slug, completeness=source.completeness, quality=source.quality,
                completeness_factors=dict(source.completeness_factors),
                quality_factors=dict(source.quality_factors), parent_index=source.index,
                version=source.version + 1,
            )
        )
        rec.write_metadata(meta)
        return RedirectResponse(f"/recordings/{recording_id}", status_code=303)

    @app.post("/recordings/{recording_id}/nodes/{node_index}/speakers")
    def remap_speakers(recording_id: str, node_index: int, speakers: str = Form(...)):
        rec = storage.get(recording_id)
        if rec is None:
            return PlainTextResponse("not found", status_code=404)
        meta = rec.read_metadata()
        node = next((chunk for chunk in meta.chunks if chunk.index == node_index), None)
        if node is None:
            return PlainTextResponse("node not found", status_code=404)
        text = rec.chunk_txt(node.index, node.slug).read_text(encoding="utf-8")
        instruction = f"Map speaker labels to these names using context: {speakers}"
        revised = llm.refine(
            dataclasses.replace(cfg.polish, timeout_s=cfg.webui.llm_timeout_s),
            text,
            instruction,
            storage.interactions_log,
            recording_id,
        )
        rec.write_refined(f"chunk-text-{node.index}", revised)
        rec.add_refinement_history(f"chunk-text-{node.index}", instruction, text, revised)
        return RedirectResponse(f"/recordings/{recording_id}", status_code=303)

    @app.get("/recordings/{recording_id}/live")
    def live(recording_id: str):
        rec = storage.get(recording_id)
        if rec is None:
            return JSONResponse({"error": "not found"}, status_code=404)
        events = []
        if storage.interactions_log.exists():
            for line in storage.interactions_log.read_text(encoding="utf-8").splitlines()[-500:]:
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if event.get("recording_id") == recording_id:
                    events.append(event)
        server_log = storage.logs_dir / "whisper-processing.log"
        whisper_output = ""
        if server_log.exists():
            lines = server_log.read_text(encoding="utf-8", errors="replace").splitlines()
            whisper_output = "\n".join(line for line in lines if recording_id in line)[-12000:]
        return JSONResponse(
            {
                "metadata": json.loads(rec.read_metadata().to_json()),
                "events": events[-100:],
                "whisper_output": whisper_output,
            }
        )

    @app.get("/api/graph")
    def graph_api():
        graph_path = storage.recordings_dir.parent / "graph.json"
        if not graph_path.exists():
            return JSONResponse({"version": 1, "nodes": [], "edges": []})
        return JSONResponse(json.loads(graph_path.read_text(encoding="utf-8")))

    @app.get("/graph", response_class=HTMLResponse)
    def graph_view():
        graph_path = storage.recordings_dir.parent / "graph.json"
        data = json.loads(graph_path.read_text(encoding="utf-8")) if graph_path.exists() else {"nodes": [], "edges": []}
        nodes = {
            node["id"]: node for node in data.get("nodes", [])
            if not node.get("archived") and not node.get("noise")
            and node.get("quality", 100) >= 35 and node.get("completeness", 100) >= 30
            and not any(term in str(node.get("title", "")).casefold().split()
                        for term in ("ambient", "indistinct", "background", "noise", "inaudible", "unclear"))
        }
        edges = [
            edge for edge in data.get("edges", [])
            if edge.get("strength", 0) >= 0.35
            and edge.get("source") in nodes and edge.get("target") in nodes
        ]
        components: list[set[str]] = []
        for edge in edges:
            matching = [group for group in components if edge["source"] in group or edge["target"] in group]
            merged = {edge["source"], edge["target"]}
            for group in matching:
                merged.update(group)
                components.remove(group)
            components.append(merged)
        components.sort(key=lambda group: sum(edge["strength"] for edge in edges if edge["source"] in group or edge["target"] in group), reverse=True)
        group_by_node = {
            node_id: index for index, group in enumerate(components, start=1) for node_id in group
        }
        title_keys = sorted({" ".join(str(node.get("title", "")).casefold().split()) for node in nodes.values()})
        title_colors = {title: index % 8 for index, title in enumerate(title_keys)}
        rendered_edges = []
        for edge in sorted(data.get("edges", []), key=lambda item: item.get("strength", 0), reverse=True):
            if edge not in edges:
                continue
            if edge["source"] in nodes and edge["target"] in nodes:
                source = nodes[edge["source"]]
                target = nodes[edge["target"]]
                source_color_index = title_colors[' '.join(str(source.get('title', '')).casefold().split())]
                target_color_index = title_colors[' '.join(str(target.get('title', '')).casefold().split())]
                source_color = f"title-color" 
                target_color = f"title-color"
                source_hue = (source_color_index * 137.508) % 360
                target_hue = (target_color_index * 137.508) % 360
                group_class = f"group-color-{group_by_node.get(edge['source'], 1) % 6}"
                rendered_edges.append((edge, 
                    f"<li class='graph-edge {group_class}'>"
                    f"<a class='graph-node {source_color}' style='--title-hue: {source_hue:.2f}' href='/recordings/{html.escape(str(source['recording_id']))}#node-{html.escape(str(source.get('index', source['id'].split(':')[-1])))}'>"
                    f"<strong>{html.escape(str(source['title']))}</strong><span>{html.escape(str(source.get('summary', ''))[:140])}</span></a>"
                    f"<span class='graph-arrow' aria-label='related'>&rarr;</span>"
                    f"<a class='graph-node {target_color}' style='--title-hue: {target_hue:.2f}' href='/recordings/{html.escape(str(target['recording_id']))}#node-{html.escape(str(target.get('index', target['id'].split(':')[-1])))}'>"
                    f"<strong>{html.escape(str(target['title']))}</strong><span>{html.escape(str(target.get('summary', ''))[:140])}</span></a>"
                    f"<span class='graph-strength'>{edge['strength']:.0%}</span></li>"))
        content = "".join(
            f"<h2 class='graph-cluster-title'>Conversation group {index}</h2>"
            f"<div class='graph-list'>{''.join(row for edge, row in rendered_edges if edge['source'] in group or edge['target'] in group)}</div>"
            for index, group in enumerate(components, start=1)
        ) or "<li>No significant related conversations yet.</li>"
        return HTMLResponse(
            "<!doctype html><html><head><meta name='viewport' content='width=device-width, initial-scale=1'>"
            "<title>byovox conversation map</title><link rel='stylesheet' href='/static/style.css?v=20260905b'>"
            "</head><body><header><div class='frame-header'><div><a href='/' class='title-link'><h1>Conversation map</h1></a></div>"
            "<button type='button' class='theme-toggle icon-button theme-fab' data-theme-toggle title='Switch theme' aria-label='Switch theme'></button></div>"
            "<p class='subtitle'>Showing only the strongest continuing conversations.</p></header>"
            f"<p><a class='button' href='/' title='Go to home'>⌂ Home</a></p>"
            f"<main><section class='graph-page'>{content}</section></main>"
            "<script src='/static/app.js?v=20260905b' defer></script></body></html>"
        )

    def _set_graph_node_archived(storage: Storage, node_id: str, archived: bool) -> None:
        graph_path = storage.recordings_dir.parent / "graph.json"
        if not graph_path.exists():
            return
        data = json.loads(graph_path.read_text(encoding="utf-8"))
        for node in data.get("nodes", []):
            if node.get("id") == node_id:
                node["archived"] = archived
        graph_path.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")

    @app.get("/prompts", response_class=HTMLResponse)
    def prompts(request: Request):
        catalog = llm.prompt_catalog(
            cfg.polish.capitalize_first_word, cfg.stt.glossary(), cfg.webui.prompts
        )
        blocks = "".join(f"<h2>{name}</h2><pre>{text}</pre>" for name, text in catalog.items())
        return HTMLResponse(f"<!doctype html><title>byovox prompts</title><body><h1>Active prompts</h1>{blocks}</body>")

    return app

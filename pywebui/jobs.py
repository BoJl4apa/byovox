"""Single sequential background worker: one job at a time is enough for a single-user tool,
and it keeps two uploads from hammering the same STT/LLM endpoints concurrently."""

from __future__ import annotations

import queue
import threading
from typing import Callable


class JobQueue:
    def __init__(self):
        self._q: "queue.Queue[Callable[[], None]]" = queue.Queue()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def submit(self, job: Callable[[], None]) -> None:
        self._q.put(job)

    def _run(self) -> None:
        while True:
            job = self._q.get()
            try:
                job()
            except Exception:  # noqa: BLE001 - a job's own code already logs its failure
                pass
            finally:
                self._q.task_done()

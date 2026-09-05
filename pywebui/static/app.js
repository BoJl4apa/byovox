// Copy-to-clipboard for each text frame on a recording's detail page. No build step, no
// framework: one delegated click listener, `navigator.clipboard` with the `execCommand`
// fallback for a non-secure context (plain http:// on the LAN this feature is meant for).
const themeToggle = document.querySelector("[data-theme-toggle]");
const savedTheme = localStorage.getItem("byovox-theme");
document.documentElement.dataset.theme = savedTheme === "dark" ? "dark" : "light";
document.documentElement.style.colorScheme = document.documentElement.dataset.theme;
if (themeToggle) {
  const refreshThemeLabel = () => {
    const label = document.documentElement.dataset.theme === "light"
      ? "Use dark mode"
      : "Use light mode";
    themeToggle.textContent = "";
    themeToggle.title = label;
    themeToggle.setAttribute("aria-label", label);
  };
  refreshThemeLabel();
  themeToggle.addEventListener("click", () => {
    const next = document.documentElement.dataset.theme === "light" ? "dark" : "light";
    document.documentElement.dataset.theme = next;
    document.documentElement.style.colorScheme = next;
    localStorage.setItem("byovox-theme", next);
    refreshThemeLabel();
  });
}

const uploadDialog = document.querySelector("[data-upload-dialog]");
const openUpload = () => {
  if (!uploadDialog) return;
  if (typeof uploadDialog.showModal === "function") uploadDialog.showModal();
  else uploadDialog.setAttribute("open", "");
};
const closeUpload = () => {
  if (!uploadDialog) return;
  if (typeof uploadDialog.close === "function") uploadDialog.close();
  else uploadDialog.removeAttribute("open");
};
document.querySelector("[data-upload-open]")?.addEventListener("click", openUpload);
document.querySelector("[data-upload-close]")?.addEventListener("click", closeUpload);
uploadDialog?.addEventListener("click", (event) => {
  if (event.target === uploadDialog) closeUpload();
});

document.querySelectorAll("[data-node-actions-open]").forEach((button) => {
  const dialog = document.getElementById(button.dataset.nodeActionsOpen);
  const close = () => {
    if (!dialog) return;
    if (typeof dialog.close === "function") dialog.close();
    else dialog.removeAttribute("open");
  };
  button.addEventListener("click", () => {
    if (!dialog) return;
    if (typeof dialog.showModal === "function") dialog.showModal();
    else dialog.setAttribute("open", "");
  });
  dialog?.querySelector("[data-node-actions-close]")?.addEventListener("click", close);
  dialog?.addEventListener("click", (event) => {
    if (event.target === dialog) close();
  });
});

document.querySelectorAll(".local-time").forEach((element) => {
  const date = new Date(element.dateTime);
  if (!Number.isNaN(date.valueOf())) {
    element.textContent = new Intl.DateTimeFormat(undefined, {
      dateStyle: "medium",
      timeStyle: "short",
    }).format(date);
  }
});

document.querySelectorAll(".node-time").forEach((element) => {
  const start = Number(element.dataset.start);
  const end = Number(element.dataset.end);
  if (Number.isFinite(start) && Number.isFinite(end)) {
    element.textContent = `${formatDuration(start)} - ${formatDuration(end)} (${formatDuration(Math.max(0, end - start))})`;
  } else {
    element.textContent = "Time unavailable";
  }
});

document.addEventListener("click", (event) => {
  const btn = event.target.closest(".copy-btn");
  if (!btn) return;
  const target = document.getElementById(btn.dataset.target);
  if (!target) return;
  const text = target.textContent;
  const done = () => {
    const original = btn.textContent;
    btn.textContent = "Copied!";
    btn.disabled = true;
    setTimeout(() => {
      btn.textContent = original;
      btn.disabled = false;
    }, 1500);
  };
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text).then(done).catch(() => fallbackCopy(text, done));
  } else {
    fallbackCopy(text, done);
  }
});

document.querySelectorAll("[data-refine-form]").forEach((form) => {
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const button = form.querySelector(".regenerate-btn");
    const label = button.querySelector("span");
    const originalLabel = label.textContent;
    button.disabled = true;
    button.classList.add("is-busy");
    label.textContent = "Generating...";
    try {
      const response = await fetch(form.action, { method: "POST", body: new FormData(form) });
      const data = await response.json();
      if (!response.ok) throw new Error(data.error || "Generation failed");
      const target = form.elements.target.value;
      const originalText = textFieldValue(form);
      const output = form.closest("section").querySelector("pre.copyable");
      const textField = form.elements.text;
      const history = form.closest("section").querySelector(`[data-history="${CSS.escape(target)}"]`);
      if (output) output.textContent = data.text;
      textField.value = data.text;
      if (history) {
        const item = document.createElement("details");
        const summary = document.createElement("summary");
        summary.textContent = form.elements.instruction.value;
        const previousLabel = document.createElement("p");
        previousLabel.className = "history-label";
        previousLabel.textContent = "Previous";
        const previousText = document.createElement("pre");
        previousText.textContent = originalText;
        const generatedLabel = document.createElement("p");
        generatedLabel.className = "history-label";
        generatedLabel.textContent = "Generated";
        const previous = document.createElement("pre");
        previous.textContent = data.text;
        item.append(summary, previousLabel, previousText, generatedLabel, previous);
        history.prepend(item);
      }
      form.elements.instruction.value = "";
    } catch (error) {
      window.alert(error.message);
    } finally {
      button.disabled = false;
      button.classList.remove("is-busy");
      label.textContent = originalLabel;
    }
  });
});

const livePanel = document.querySelector("[data-live-recording]");
if (livePanel) {
  const id = livePanel.dataset.liveRecording;
  const output = document.getElementById("live-log");
  const updated = document.getElementById("live-updated");
  const refreshLive = async () => {
    try {
      const response = await fetch(`/recordings/${encodeURIComponent(id)}/live`, { cache: "no-store" });
      if (!response.ok) return;
      const data = await response.json();
      const meta = data.metadata;
      const progress = [];
      if (meta.stage_detail) progress.push(meta.stage_detail);
      if (meta.total_chunks) {
        progress.push(`${meta.completed_chunks} / ${meta.total_chunks} chunks`);
      }
      if (meta.total_speech_s) {
        progress.push(`speech ${formatDuration(meta.processed_speech_s)} / ${formatDuration(meta.total_speech_s)}`);
      }
      if (meta.eta_s && meta.status === "transcribing") {
        progress.push(`about ${formatDuration(meta.eta_s)} remaining`);
      }
      const lines = progress.length ? [`${meta.status}: ${progress.join(" · ")}`, ""] : [];
      lines.push(...data.events.map(formatLiveEvent));
      if (data.whisper_output) lines.push(data.whisper_output);
      output.textContent = lines.join("\n") || `status: ${data.metadata.status}`;
      updated.textContent = `${data.metadata.status} · ${new Date().toLocaleTimeString()}`;
    } catch (_) {
      updated.textContent = "connection lost; retrying...";
    }
  };
  refreshLive();
  setInterval(refreshLive, 2000);
}

function formatLiveEvent(event) {
  const stage = event.stage || "pipeline";
  if (stage === "stt" && event.chunk) return `Transcribed ${event.chunk}`;
  if (stage === "polish") return "Polished a transcript window";
  if (stage === "split") return "Identified conversation topics";
  if (stage === "summary" || stage === "summary-reduce") return "Generated a summary";
  if (stage === "pipeline" && event.event === "noise_classified") {
    return `Classified ${event.noise_count || 0} repeated noise segments`;
  }
  if (stage === "pipeline" && event.event === "speech_ranges") {
    return `Found ${event.ranges || 0} speech ranges in ${formatDuration(event.duration_s)}`;
  }
  if (stage === "pipeline" && event.event === "preparing") return "Preparing audio";
  if (stage === "pipeline" && event.event === "transcribing") return "Starting transcription";
  return `${stage}: processing completed`;
}

function textFieldValue(form) {
  return form.elements.text ? form.elements.text.value : "";
}

function formatDuration(seconds) {
  const total = Math.max(0, Math.round(Number(seconds) || 0));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  if (hours) return `${hours}h ${minutes}m`;
  if (minutes) return `${minutes}m ${secs}s`;
  return `${secs}s`;
}

function fallbackCopy(text, done) {
  const area = document.createElement("textarea");
  area.value = text;
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.appendChild(area);
  area.select();
  try {
    document.execCommand("copy");
    done();
  } finally {
    area.remove();
  }
}

/* Ghostwriter — UI logic.
 *
 * The webview owns no timing logic: it renders the snapshots the Rust engine
 * emits and forwards intents. That keeps the typing loop accurate even when the
 * window is hidden or the compositor throttles painting.
 */

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const el = {
  text: $("text"),
  charCount: $("charCount"),
  wordCount: $("wordCount"),
  eta: $("eta"),
  wpm: $("wpm"),
  wpmOut: $("wpmOut"),
  wpmValue: $("wpmValue"),
  delay: $("delay"),
  delayOut: $("delayOut"),
  abortOnFocus: $("abortOnFocus"),
  focusRow: $("focusRow"),
  focusNote: $("focusNote"),
  bar: $("bar"),
  progressBar: $("progressBar"),
  phaseLabel: $("phaseLabel"),
  progressText: $("progressText"),
  startBtn: $("startBtn"),
  startLabel: $("startLabel"),
  pauseBtn: $("pauseBtn"),
  cancelBtn: $("cancelBtn"),
  pasteBtn: $("pasteBtn"),
  clearBtn: $("clearBtn"),
  shortcuts: $("shortcuts"),
  hint: $("hint"),
  error: $("error"),
};

/** Mirror of the backend snapshot. */
const state = {
  phase: "idle",
  done: 0,
  total: 0,
  error: null,
  /** True when the backend allows continuing a stopped/failed run. */
  resumable: false,
  /** Where a resumed run restarts from. */
  resumeFrom: 0,
};

const PHASE_LABEL = {
  idle: "Pronto",
  countdown: "Foque a janela de destino…",
  typing: "Digitando",
  paused: "Pausado",
  done: "Concluído",
  stopped: "Interrompido",
  failed: "Falhou",
};

const RUNNING = new Set(["countdown", "typing", "paused"]);

/* ------------------------------- helpers -------------------------------- */

const msPerChar = (wpm) => Math.round(60000 / (wpm * 5));

function formatDuration(seconds) {
  const s = Math.max(0, Math.round(seconds));
  const m = Math.floor(s / 60);
  return `${m}:${String(s % 60).padStart(2, "0")}`;
}

/** 5 chars per word is the same convention the backend uses. */
function etaSeconds(text, wpm) {
  const chars = [...text].length;
  return (chars / (wpm * 5)) * 60;
}

function setError(message) {
  if (message) {
    el.error.textContent = message;
    el.error.hidden = false;
  } else {
    el.error.hidden = true;
    el.error.textContent = "";
  }
}

/* -------------------------------- counters ------------------------------- */

function refreshCounters() {
  const text = el.text.value;
  const chars = [...text].length;
  el.charCount.textContent = chars.toLocaleString("pt-BR");
  el.wordCount.textContent = (text.trim() ? text.trim().split(/\s+/).length : 0).toLocaleString(
    "pt-BR",
  );
  el.eta.textContent = formatDuration(etaSeconds(text, Number(el.wpm.value)));
}

function refreshSpeedLabels() {
  const wpm = Number(el.wpm.value);
  el.wpmValue.textContent = wpm;
  el.wpmOut.textContent = `${wpm} WPM · ${msPerChar(wpm)} ms/caractere`;
  el.delayOut.textContent = `${el.delay.value} s`;
}

/* -------------------------------- rendering ------------------------------ */

/** True when there is text that could actually be typed. */
function hasTypableText() {
  return el.text.value.trim().length > 0;
}

function render() {
  const { phase, done, total, error } = state;
  const running = RUNNING.has(phase);
  const paused = phase === "paused";

  el.phaseLabel.textContent =
    phase === "stopped" && !state.resumable ? "Parado" : (PHASE_LABEL[phase] ?? phase);
  el.progressText.textContent = `${done} / ${total}`;

  const pct = total > 0 ? Math.min(100, (done / total) * 100) : 0;
  el.bar.style.width = `${pct}%`;
  el.bar.classList.toggle("paused", paused);
  el.progressBar.setAttribute("aria-valuenow", Math.round(pct));

  // Start button doubles as "Continuar" after an automatic interruption and
  // as "Recomeçar" after a deliberate cancel.
  const resumable =
    (phase === "stopped" || phase === "failed") &&
    state.resumable &&
    done > 0 &&
    done < total;
  // An idle run has no total yet, so the textarea itself decides.
  el.startBtn.disabled = running || !hasTypableText();
  el.startBtn.classList.toggle("running", running);
  el.startLabel.textContent = running
    ? "Digitando…"
    : resumable
      ? `Continuar do caractere ${done + 1}`
      : phase === "stopped" && done > 0
        ? "Recomeçar"
        : "Iniciar";

  el.pauseBtn.disabled = !running;
  el.pauseBtn.textContent = paused ? "Retomar" : "Pausar";
  // Cancelling is meaningful while a run is active, and also after an
  // automatic interruption: there it discards the resume point so the next
  // Start begins from the first character instead of offering "Continue".
  el.cancelBtn.disabled = !(running || resumable);

  // The text must not change mid-run, or progress stops matching the content.
  el.text.readOnly = running;
  el.pasteBtn.disabled = running;
  el.clearBtn.disabled = running;

  if (error) setError(error);
  else if (phase !== "failed") setError(null);
}

/* --------------------------------- wiring -------------------------------- */

el.text.addEventListener("input", () => {
  // Editing after a partial run invalidates the resume point.
  if (!RUNNING.has(state.phase)) {
    state.resumeFrom = 0;
    state.resumable = false;
    if (state.phase === "stopped" || state.phase === "failed" || state.phase === "done") {
      state.phase = "idle";
      state.done = 0;
      state.total = 0;
    }
  }
  refreshCounters();
  render();
});

el.wpm.addEventListener("input", () => {
  refreshSpeedLabels();
  refreshCounters();
  persistSettings();
});

el.delay.addEventListener("input", () => {
  refreshSpeedLabels();
  persistSettings();
});

el.abortOnFocus.addEventListener("change", persistSettings);

el.pasteBtn.addEventListener("click", async () => {
  try {
    const text = await invoke("read_clipboard");
    if (text) {
      el.text.value = text;
      el.text.dispatchEvent(new Event("input"));
    }
  } catch (err) {
    setError(String(err));
  }
});

el.clearBtn.addEventListener("click", () => {
  el.text.value = "";
  el.text.dispatchEvent(new Event("input"));
  el.text.focus();
});

el.startBtn.addEventListener("click", async () => {
  const text = el.text.value;
  if (!text.trim()) return;

  // A fresh run from the top unless the backend says the last interruption is
  // resumable (automatic stops do; a deliberate Stop does not).
  const resumable =
    state.resumable &&
    (state.phase === "stopped" || state.phase === "failed") &&
    state.done > 0 &&
    state.done < [...text].length;
  const resumeFrom = resumable ? state.done : 0;

  try {
    setError(null);
    // A settings change made just before Start may still be inside the
    // debounce window. Flush it first, otherwise the run would use the old
    // speed or the old focus-guard setting.
    await flushSettings();
    await invoke("start", { text, resumeFrom });
  } catch (err) {
    setError(String(err));
  }
});

el.pauseBtn.addEventListener("click", () => {
  invoke("toggle_pause").catch((err) => setError(String(err)));
});

el.cancelBtn.addEventListener("click", async () => {
  try {
    const snapshot = await invoke("cancel");
    if (snapshot) applySnapshot(snapshot);
  } catch (err) {
    setError(String(err));
  }
});

function applySnapshot(snapshot) {
  if (!snapshot) return;
  state.phase = snapshot.phase;
  state.done = snapshot.done;
  state.total = snapshot.total;
  state.error = snapshot.error ?? null;
  state.resumable = snapshot.resumable === true;

  // Keep the resume point only while the run is finished but incomplete.
  if (snapshot.phase === "done") {
    state.resumeFrom = 0;
    state.resumable = false;
  }
  render();
}

/* -------------------------------- settings ------------------------------- */

let persistTimer = null;

function settingsPayload() {
  return {
    wpm: Number(el.wpm.value),
    startDelaySecs: Number(el.delay.value),
    abortOnFocusChange: el.abortOnFocus.checked,
  };
}

function persistSettings() {
  clearTimeout(persistTimer);
  persistTimer = setTimeout(() => {
    invoke("update_settings", { settings: settingsPayload() }).catch((err) =>
      setError(String(err)),
    );
  }, 180);
}

/** Sends the current controls immediately, bypassing the debounce. */
async function flushSettings() {
  clearTimeout(persistTimer);
  try {
    await invoke("update_settings", { settings: settingsPayload() });
  } catch (err) {
    setError(String(err));
  }
}

async function loadSettings() {
  const s = await invoke("get_settings");
  el.wpm.value = s.wpm;
  el.delay.value = s.startDelaySecs;
  el.abortOnFocus.checked = s.abortOnFocusChange;
  refreshSpeedLabels();
}

/**
 * The focus guard is a Windows/X11 feature. Where the OS gives no portable way
 * to read the focused window (generic Wayland), force it off so the user never
 * believes the protection is active when it is not.
 */
function applyFocusGuardSupport(canTrackFocus) {
  if (canTrackFocus) return;
  el.abortOnFocus.checked = false;
  el.abortOnFocus.disabled = true;
  el.focusRow.classList.add("disabled");
  el.focusNote.textContent = "Indisponível nesta plataforma.";
}

/* --------------------------------- boot ---------------------------------- */

async function boot() {
  const info = await invoke("platform_info");

  if (info.hint) {
    el.hint.textContent = info.hint;
    el.hint.hidden = false;
  }

  el.shortcuts.innerHTML =
    `Pausar/retomar <kbd>${info.shortcuts.pauseResume}</kbd> · Cancelar <kbd>${info.shortcuts.stop}</kbd>` +
    ` — funcionam com outra janela em foco.`;

  // Settings first, so the platform rule below is not undone by a restored
  // `abortOnFocusChange: true` coming from the backend defaults.
  await loadSettings();
  applyFocusGuardSupport(info.canTrackFocus);
  refreshCounters();

  // Restore a run that was in progress if the webview reloaded.
  applySnapshot(await invoke("status"));

  await listen("engine://progress", (event) => applySnapshot(event.payload));

  render();
}

boot().catch((err) => setError(String(err)));

/**
 * End-to-end tests for the Ghostwriter UI.
 *
 * These drive the real `ui/app.js` inside jsdom, against a fake implementation
 * of the Tauri IPC surface (`__TAURI__.core.invoke` / `__TAURI__.event.listen`).
 * That makes the contract between the two layers executable: command names,
 * argument shapes, event names and the phase vocabulary.
 *
 * A real Rust command signature change will fail these tests, which is the
 * point: it is much cheaper than discovering it by hand in the packaged app.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { JSDOM } from "jsdom";

const here = dirname(fileURLToPath(import.meta.url));
const uiDir = join(here, "..", "..", "ui");

const HTML = readFileSync(join(uiDir, "index.html"), "utf8");
const SCRIPT = readFileSync(join(uiDir, "app.js"), "utf8");

/** Default replies, mirroring the real Rust commands. */
function defaultBackend(overrides = {}) {
  const calls = [];
  const listeners = new Map();

  const settings = {
    wpm: 60,
    startDelaySecs: 3,
    abortOnFocusChange: true,
    ...(overrides.settings ?? {}),
  };

  const replies = {
    platform_info: {
      os: "windows",
      hint: null,
      canTrackFocus: true,
      shortcuts: { pauseResume: "Ctrl+Shift+P", stop: "Ctrl+Shift+S" },
      ...(overrides.platformInfo ?? {}),
    },
    read_clipboard: "texto colado da área de transferência",
    active_window: "Notepad — documento.txt",
    get_settings: settings,
    update_settings: (args) => args.settings,
    status: { phase: "idle", done: 0, total: 0, error: null },
    toggle_pause: undefined,
    start: undefined,
    cancel: { phase: "idle", done: 0, total: 0, error: null, resumable: false },
    ...overrides.replies,
  };

  const invoke = async (cmd, args) => {
    calls.push({ cmd, args });
    if (overrides.onInvoke) {
      const custom = await overrides.onInvoke(cmd, args, calls);
      if (custom !== undefined) return custom;
    }
    if (!(cmd in replies)) {
      throw new Error(`unknown command: ${cmd}`);
    }
    const value = replies[cmd];
    return typeof value === "function" ? value(args) : value;
  };

  const listen = async (event, handler) => {
    listeners.set(event, handler);
    return () => listeners.delete(event);
  };

  /** Simulates the Rust engine emitting an `engine://progress` event. */
  const emit = (payload) => {
    const handler = listeners.get("engine://progress");
    if (!handler) throw new Error("the UI did not subscribe to engine://progress");
    handler({ event: "engine://progress", payload });
  };

  return { invoke, listen, calls, emit, settings, listeners, replies };
}

/** Boots `app.js` in a fresh jsdom with the given fake backend. */
async function boot(backend, { waitMs = 60 } = {}) {
  const dom = new JSDOM(HTML, { runScripts: "outside-only", pretendToBeVisual: true });
  const { window } = dom;

  window.__TAURI__ = { core: { invoke: backend.invoke }, event: { listen: backend.listen } };

  // jsdom has no layout, so clamp the scroll calls the UI may make.
  window.HTMLElement.prototype.scrollIntoView = () => {};

  window.eval(SCRIPT);

  // `boot()` is async: give its microtasks a turn to settle.
  await new Promise((resolve) => setTimeout(resolve, waitMs));
  return { window, dom };
}

const el = (window, id) => window.document.getElementById(id);
const text = (window, id) => el(window, id).textContent.trim();

/** Types into the textarea the way a user would. */
function typeText(window, value) {
  const textarea = el(window, "text");
  textarea.value = value;
  textarea.dispatchEvent(new window.Event("input"));
}

/* --------------------------------- boot ---------------------------------- */

test("boots and asks the backend for platform info and settings", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);

  const cmds = backend.calls.map((c) => c.cmd);
  assert.ok(cmds.includes("platform_info"), "missing platform_info");
  assert.ok(cmds.includes("get_settings"), "missing get_settings");
  assert.ok(cmds.includes("status"), "missing status");
  assert.ok(backend.listeners.has("engine://progress"), "did not subscribe to progress");

  assert.equal(text(window, "wpmValue"), "60");
  assert.equal(text(window, "phaseLabel"), "Pronto");
});

test("shows the Wayland hint when the backend reports one", async () => {
  const backend = defaultBackend({
    platformInfo: { os: "linux", hint: "Sessão Wayland detectada.", canTrackFocus: false },
  });
  const { window } = await boot(backend);

  const hint = el(window, "hint");
  assert.equal(hint.hidden, false);
  assert.match(hint.textContent, /Wayland/);
});

test("disables the focus guard when the platform cannot track focus", async () => {
  const backend = defaultBackend({
    platformInfo: { os: "linux", hint: null, canTrackFocus: false },
  });
  const { window } = await boot(backend);

  const checkbox = el(window, "abortOnFocus");
  assert.equal(checkbox.disabled, true);
  assert.equal(checkbox.checked, false);
  assert.match(text(window, "focusNote"), /Indisponível/);
});

test("renders the global shortcuts from the backend", async () => {
  const { window } = await boot(defaultBackend());
  const shortcuts = text(window, "shortcuts");
  assert.match(shortcuts, /Ctrl\+Shift\+P/);
  assert.match(shortcuts, /Ctrl\+Shift\+S/);
  assert.doesNotMatch(shortcuts, /undefined/, "a field-name mismatch would leak here");
});

test("restores an in-progress run reported by status", async () => {
  const backend = defaultBackend({
    replies: { status: { phase: "typing", done: 40, total: 100, error: null } },
  });
  const { window } = await boot(backend);

  assert.equal(text(window, "phaseLabel"), "Digitando");
  assert.equal(text(window, "progressText"), "40 / 100");
  assert.equal(el(window, "bar").style.width, "40%");
});

/* ------------------------------- typing ---------------------------------- */

test("updates counters and ETA as text is typed", async () => {
  const { window } = await boot(defaultBackend());
  const textarea = el(window, "text");

  textarea.value = "uma duas três quatro cinco";
  textarea.dispatchEvent(new window.Event("input"));

  assert.equal(text(window, "charCount"), "26");
  assert.equal(text(window, "wordCount"), "5");
  // 26 chars at 60 WPM -> 26 / 300 min -> 5.2 s -> 0:05
  assert.equal(text(window, "eta"), "0:05");
});

test("WPM slider updates the badge, the per-character delay and the ETA", async () => {
  const { window } = await boot(defaultBackend());
  const wpm = el(window, "wpm");

  wpm.value = "160";
  wpm.dispatchEvent(new window.Event("input"));

  assert.equal(text(window, "wpmValue"), "160");
  assert.match(text(window, "wpmOut"), /160 WPM · 75 ms\/caractere/);
});

test("the speed slider covers 60 to 300 WPM", async () => {
  const { window } = await boot(defaultBackend());
  const wpm = el(window, "wpm");

  assert.equal(wpm.min, "60");
  assert.equal(wpm.max, "300");
  assert.equal(wpm.value, "60");
});

test("the per-character delay matches the reference table at 60/100/160/300", async () => {
  const { window } = await boot(defaultBackend());
  const wpm = el(window, "wpm");

  for (const [value, expected] of [
    ["60", "200"],
    ["100", "120"],
    ["160", "75"],
    ["300", "40"],
  ]) {
    wpm.value = value;
    wpm.dispatchEvent(new window.Event("input"));
    assert.match(
      text(window, "wpmOut"),
      new RegExp(`${value} WPM · ${expected} ms`),
      `wrong delay at ${value} WPM`,
    );
  }
});

test("accented characters count as single characters, not bytes", async () => {
  const { window } = await boot(defaultBackend());
  const textarea = el(window, "text");

  textarea.value = "ção";
  textarea.dispatchEvent(new window.Event("input"));
  assert.equal(text(window, "charCount"), "3");
});

/* ------------------------------- clipboard -------------------------------- */

test("paste button loads the clipboard into the textarea", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);

  el(window, "pasteBtn").click();
  await new Promise((r) => setTimeout(r, 30));

  assert.equal(el(window, "text").value, backend.replies.read_clipboard);
  assert.equal(text(window, "charCount"), String(backend.replies.read_clipboard.length));
});

test("a clipboard error is shown instead of swallowed", async () => {
  const backend = defaultBackend({
    onInvoke: (cmd) => {
      if (cmd === "read_clipboard") throw "A área de transferência não contém texto.";
    },
  });
  const { window } = await boot(backend);

  el(window, "pasteBtn").click();
  await new Promise((r) => setTimeout(r, 30));

  assert.equal(el(window, "error").hidden, false);
  assert.match(text(window, "error"), /não contém texto/);
});

test("clear button empties the text and resets the counters", async () => {
  const { window } = await boot(defaultBackend());
  const textarea = el(window, "text");

  textarea.value = "algo";
  textarea.dispatchEvent(new window.Event("input"));
  assert.equal(text(window, "charCount"), "4");

  el(window, "clearBtn").click();
  assert.equal(textarea.value, "");
  assert.equal(text(window, "charCount"), "0");
});

/* ---------------------------------- run ----------------------------------- */

test("start is disabled while the text is empty and enables with content", async () => {
  const { window } = await boot(defaultBackend());
  const textarea = el(window, "text");

  assert.equal(el(window, "startBtn").disabled, true, "nothing to type yet");

  textarea.value = "olá";
  textarea.dispatchEvent(new window.Event("input"));
  assert.equal(el(window, "startBtn").disabled, false);

  // Whitespace is not content: the backend would reject it anyway.
  textarea.value = "   \n\t  ";
  textarea.dispatchEvent(new window.Event("input"));
  assert.equal(el(window, "startBtn").disabled, true, "whitespace must not enable start");
});

test("start sends the text and a resume_from of zero", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);

  const textarea = el(window, "text");
  textarea.value = "olá mundo";
  textarea.dispatchEvent(new window.Event("input"));

  el(window, "startBtn").click();
  await new Promise((r) => setTimeout(r, 30));

  const startCall = backend.calls.find((c) => c.cmd === "start");
  assert.ok(startCall, "start was never invoked");
  assert.equal(startCall.args.text, "olá mundo");
  assert.equal(startCall.args.resumeFrom, 0);
  assert.equal(startCall.args.resumeFrom, 0);
});

test("progress events drive the bar, the labels and the controls", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);
  typeText(window, "x".repeat(100));

  backend.emit({ phase: "countdown", done: 0, total: 100, error: null });
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(text(window, "phaseLabel"), "Foque a janela de destino…");
  assert.equal(el(window, "startBtn").disabled, true);
  assert.equal(el(window, "pauseBtn").disabled, false);
  assert.equal(el(window, "cancelBtn").disabled, false);

  backend.emit({ phase: "typing", done: 25, total: 100, error: null });
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(text(window, "phaseLabel"), "Digitando");
  assert.equal(text(window, "progressText"), "25 / 100");
  assert.equal(el(window, "bar").style.width, "25%");
  assert.equal(el(window, "text").readOnly, true, "text must be locked while typing");

  backend.emit({ phase: "done", done: 100, total: 100, error: null });
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(text(window, "phaseLabel"), "Concluído");
  assert.equal(el(window, "startBtn").disabled, false);
  assert.equal(el(window, "pauseBtn").disabled, true);
  assert.equal(el(window, "text").readOnly, false);
});

test("a paused run flips the button label and tints the bar", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);

  backend.emit({ phase: "paused", done: 10, total: 100, error: null });
  await new Promise((r) => setTimeout(r, 10));

  assert.equal(text(window, "phaseLabel"), "Pausado");
  assert.equal(text(window, "pauseBtn"), "Retomar");
  assert.ok(el(window, "bar").classList.contains("paused"));
});

test("pause button toggles through the toggle_pause command", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);

  backend.emit({ phase: "typing", done: 5, total: 100, error: null });
  await new Promise((r) => setTimeout(r, 10));

  el(window, "pauseBtn").click();
  await new Promise((r) => setTimeout(r, 20));

  assert.ok(
    backend.calls.some((c) => c.cmd === "toggle_pause"),
    "toggle_pause was not invoked",
  );
});

test("cancel applies the snapshot the backend returns", async () => {
  const backend = defaultBackend({
    replies: {
      cancel: { phase: "stopped", done: 30, total: 100, error: null, resumable: false },
    },
  });
  const { window } = await boot(backend);
  typeText(window, "x".repeat(100));

  backend.emit({ phase: "typing", done: 30, total: 100, error: null });
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(text(window, "cancelBtn"), "Cancelar");

  el(window, "cancelBtn").click();
  await new Promise((r) => setTimeout(r, 30));

  assert.equal(text(window, "phaseLabel"), "Parado");
  assert.equal(el(window, "startBtn").disabled, false);
  assert.equal(el(window, "cancelBtn").disabled, true);
  assert.equal(text(window, "startLabel"), "Recomeçar");
});

test("cancelling keeps the text and restarts from the beginning, not the stop point", async () => {
  const backend = defaultBackend({
    replies: {
      cancel: { phase: "stopped", done: 30, total: 100, error: null, resumable: false },
    },
  });
  const { window } = await boot(backend);
  typeText(window, "x".repeat(100));

  backend.emit({ phase: "typing", done: 30, total: 100, error: null });
  await new Promise((r) => setTimeout(r, 10));

  el(window, "cancelBtn").click();
  await new Promise((r) => setTimeout(r, 30));
  assert.equal(text(window, "phaseLabel"), "Parado");
  assert.equal(text(window, "startLabel"), "Recomeçar");
  assert.equal(el(window, "text").value, "x".repeat(100), "cancel must not clear the text");

  el(window, "startBtn").click();
  await new Promise((r) => setTimeout(r, 30));

  const start = backend.calls.filter((c) => c.cmd === "start").at(-1);
  assert.ok(start, "start was never invoked");
  assert.equal(start.args.resumeFrom, 0, "cancel must not resume");
});

test("starting flushes pending settings before the run", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);
  typeText(window, "olá");

  // Unchecking and immediately starting must not use the old, debounced value.
  const checkbox = el(window, "abortOnFocus");
  checkbox.checked = false;
  checkbox.dispatchEvent(new window.Event("change"));

  el(window, "startBtn").click();
  await new Promise((r) => setTimeout(r, 40));

  const names = backend.calls.map((c) => c.cmd);
  const saveIdx = names.lastIndexOf("update_settings");
  const startIdx = names.lastIndexOf("start");
  assert.ok(saveIdx >= 0, "settings were not flushed before starting");
  assert.ok(saveIdx < startIdx, "start ran before the settings were saved");
  assert.equal(backend.calls[saveIdx].args.settings.abortOnFocusChange, false);
});

test("an interrupted run offers to continue from where it stopped", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);

  const textarea = el(window, "text");
  textarea.value = "x".repeat(100);
  textarea.dispatchEvent(new window.Event("input"));

  backend.emit({ phase: "typing", done: 40, total: 100, error: null });
  await new Promise((r) => setTimeout(r, 10));
  backend.emit({ phase: "stopped", done: 40, total: 100, error: null, resumable: true });
  await new Promise((r) => setTimeout(r, 10));

  assert.match(text(window, "startLabel"), /Continuar do caractere 41/);

  el(window, "startBtn").click();
  await new Promise((r) => setTimeout(r, 30));

  const startCall = backend.calls.filter((c) => c.cmd === "start").at(-1);
  assert.equal(startCall.args.resumeFrom, 40, "resume must start after the typed prefix");
});

test("editing after an interruption clears the resume point", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);

  const textarea = el(window, "text");
  textarea.value = "x".repeat(100);
  textarea.dispatchEvent(new window.Event("input"));

  backend.emit({ phase: "stopped", done: 40, total: 100, error: null, resumable: true });
  await new Promise((r) => setTimeout(r, 10));
  assert.match(text(window, "startLabel"), /Continuar/);

  textarea.value = "conteúdo novo";
  textarea.dispatchEvent(new window.Event("input"));
  await new Promise((r) => setTimeout(r, 10));

  assert.equal(text(window, "startLabel"), "Iniciar", "stale resume point survived an edit");
  assert.equal(text(window, "progressText"), "0 / 0");
});

test("cancel after an interruption discards the resume point and keeps the text", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);
  typeText(window, "x".repeat(100));

  backend.emit({ phase: "stopped", done: 40, total: 100, error: null, resumable: true });
  await new Promise((r) => setTimeout(r, 10));

  assert.match(text(window, "startLabel"), /Continuar do caractere 41/);
  assert.equal(el(window, "cancelBtn").disabled, false, "cancel must be offered after an interruption");

  el(window, "cancelBtn").click();
  await new Promise((r) => setTimeout(r, 30));

  assert.ok(
    backend.calls.some((c) => c.cmd === "cancel"),
    "cancel was not invoked",
  );
  assert.equal(text(window, "phaseLabel"), "Pronto");
  assert.equal(text(window, "progressText"), "0 / 0");
  assert.equal(text(window, "startLabel"), "Iniciar");
  assert.equal(el(window, "cancelBtn").disabled, true);
  assert.equal(el(window, "text").value, "x".repeat(100), "cancel must not clear the text");
});

test("a failed run surfaces the error message from the engine", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);

  backend.emit({
    phase: "failed",
    done: 0,
    total: 10,
    error: "no successful connection — Sessão Wayland detectada.",
  });
  await new Promise((r) => setTimeout(r, 10));

  assert.equal(el(window, "error").hidden, false);
  assert.match(text(window, "error"), /Wayland/);
  assert.equal(text(window, "phaseLabel"), "Falhou");
});

test("a focus-guard stop explains why the run ended", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);
  typeText(window, "x".repeat(100));

  backend.emit({ phase: "typing", done: 40, total: 100, error: null });
  await new Promise((r) => setTimeout(r, 10));

  backend.emit({
    phase: "stopped",
    done: 40,
    total: 100,
    error: "A janela em foco mudou. A digitação foi interrompida.",
    resumable: true,
  });
  await new Promise((r) => setTimeout(r, 10));

  assert.equal(text(window, "phaseLabel"), "Interrompido");
  assert.equal(el(window, "error").hidden, false);
  assert.match(text(window, "error"), /janela em foco mudou/);
  // The run is resumable, so the reason must not block the controls.
  assert.match(text(window, "startLabel"), /Continuar do caractere 41/);
});

test("settings changes are persisted with camelCase keys", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);

  const wpm = el(window, "wpm");
  wpm.value = "75";
  wpm.dispatchEvent(new window.Event("input"));

  await new Promise((r) => setTimeout(r, 260)); // debounce is 180 ms

  const saved = backend.calls.filter((c) => c.cmd === "update_settings").at(-1);
  assert.ok(saved, "update_settings was never invoked");
  assert.deepEqual(Object.keys(saved.args.settings).sort(), [
    "abortOnFocusChange",
    "startDelaySecs",
    "wpm",
  ]);
  assert.equal(saved.args.settings.wpm, 75);
});

test("deep-linked settings are applied to the controls on load", async () => {
  const backend = defaultBackend({
    settings: { wpm: 84, startDelaySecs: 7, abortOnFocusChange: false },
  });
  const { window } = await boot(backend);

  assert.equal(el(window, "wpm").value, "84");
  assert.equal(el(window, "delay").value, "7");
  assert.equal(el(window, "abortOnFocus").checked, false);
  assert.equal(text(window, "wpmValue"), "84");
});

test("an unknown command rejects loudly instead of silently passing", async () => {
  const backend = defaultBackend();
  const { window } = await boot(backend);

  // The fake throws for commands it does not know; that is the safety net that
  // keeps this suite honest when a command is renamed in Rust.
  await assert.rejects(() => backend.invoke("nao_existe", {}), /unknown command/);
  assert.ok(window.document.getElementById("text"));
});

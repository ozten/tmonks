// tmonks frontend — sidebar + focused-pane WebSocket client.
//
// CSP: served as `script-src 'self'`. No inline scripts. The page loads
// `xterm.js`, `addon-fit`, `addon-web-links`, and `addon-search` from
// `/assets/vendor/`.
//
// Binary protocol (see src/ws_pane.rs):
//
//   Server → client tags:
//     0x01  seed     — initial scrollback; client calls term.reset() first
//     0x02  live     — appended pane bytes
//     0x13  scrollback-response (Unit 7 copy-all button)
//
//   Client → server tags:
//     0x10  stdin    — keystrokes / pasted bytes
//     0x11  resize   — [cols u16 BE][rows u16 BE]
//     0x12  request-scrollback

import { Terminal } from "/assets/vendor/xterm.mjs";
import { FitAddon } from "/assets/vendor/addon-fit.mjs";
import { WebLinksAddon } from "/assets/vendor/addon-web-links.mjs";
import { SearchAddon } from "/assets/vendor/addon-search.mjs";

const $ = (sel) => document.querySelector(sel);

/** Human-readable labels per the plan's badge spec. */
const STATUS_LABELS = {
  idle: "idle",
  working: "working",
  "needs-input": "needs input",
  "idle-notify": "waiting on you",
  unknown: "unknown",
};

// ---- Pane WebSocket client --------------------------------------------------

/** WebSocket reconnect backoff steps (ms). After all 5 exhausted, give up. */
const RECONNECT_BACKOFFS_MS = [200, 400, 800, 1600, 3200];

class PaneClient {
  constructor(term, fit, sessionId) {
    this.term = term;
    this.fit = fit;
    this.sessionId = sessionId;
    this.ws = null;
    this.pendingScrollback = null;
    this._onData = null;
    this._resizeDebounce = null;
    this._lastReportedSize = null;
    this._reconnectAttempt = 0;
    this._stopped = false;
  }

  connect() {
    this._stopped = false;
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${proto}//${location.host}/ws/pane/${encodeURIComponent(this.sessionId)}`;
    this.ws = new WebSocket(url);
    this.ws.binaryType = "arraybuffer";
    this._sawAnyMessage = false;
    this.ws.addEventListener("open", () => {
      this._reconnectAttempt = 0;
      hideRetryStrip();
      this._onOpen();
    });
    this.ws.addEventListener("message", (ev) => { this._sawAnyMessage = true; this._onMessage(ev); });
    this.ws.addEventListener("close", (ev) => this._onClose(ev));
    this.ws.addEventListener("error", (ev) => this._onError(ev));
  }

  close() {
    this._stopped = true;
    if (this._onData) {
      this._onData.dispose();
      this._onData = null;
    }
    if (this.ws && this.ws.readyState <= WebSocket.OPEN) {
      this.ws.close();
    }
    this.ws = null;
    hideRetryStrip();
  }

  _onOpen() {
    // Hook xterm.js onData → 0x10 stdin frame.
    this._onData = this.term.onData((str) => {
      const bytes = new TextEncoder().encode(str);
      const frame = new Uint8Array(bytes.length + 1);
      frame[0] = 0x10;
      frame.set(bytes, 1);
      if (this.ws && this.ws.readyState === WebSocket.OPEN) {
        this.ws.send(frame);
      }
    });

    // Report current size (cols × rows).
    this._sendResize();
  }

  _sendResize() {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const cols = this.term.cols;
    const rows = this.term.rows;
    if (this._lastReportedSize &&
        this._lastReportedSize.cols === cols &&
        this._lastReportedSize.rows === rows) return;
    this._lastReportedSize = { cols, rows };

    const frame = new Uint8Array(5);
    frame[0] = 0x11;
    frame[1] = (cols >> 8) & 0xff;
    frame[2] = cols & 0xff;
    frame[3] = (rows >> 8) & 0xff;
    frame[4] = rows & 0xff;
    this.ws.send(frame);
  }

  /** Debounced resize. Unit 6 calls this from a ResizeObserver. */
  onContainerResize() {
    clearTimeout(this._resizeDebounce);
    this._resizeDebounce = setTimeout(() => {
      try { this.fit.fit(); } catch (_) {}
      this._sendResize();
    }, 100);
  }

  /** Returns a Promise resolving to the full scrollback bytes. */
  requestScrollback() {
    if (this.pendingScrollback) {
      return this.pendingScrollback;
    }
    this.pendingScrollback = new Promise((resolve, reject) => {
      // Slightly longer than the server's 12s capture-pane budget — lets the
      // server's error frame arrive before the client gives up.
      const timer = setTimeout(() => {
        this.pendingScrollback = null;
        reject(new Error("scrollback request timed out"));
      }, 13000);
      this._scrollbackResolver = (bytes) => {
        clearTimeout(timer);
        this.pendingScrollback = null;
        resolve(bytes);
      };
      const frame = new Uint8Array([0x12]);
      this.ws.send(frame);
    });
    return this.pendingScrollback;
  }

  _onMessage(ev) {
    if (typeof ev.data === "string") {
      // Text frame: JSON error.
      try {
        const body = JSON.parse(ev.data);
        if (body.err) {
          showToast(`Pane error: ${body.err}`);
        }
      } catch (_) {}
      return;
    }
    const buf = new Uint8Array(ev.data);
    if (buf.length === 0) return;
    const tag = buf[0];
    const payload = buf.subarray(1);
    switch (tag) {
      case 0x01: { // seed
        this.term.reset();
        this.term.write(payload);
        break;
      }
      case 0x02: { // live
        this.term.write(payload);
        break;
      }
      case 0x13: { // scrollback-response
        if (this._scrollbackResolver) {
          this._scrollbackResolver(payload);
          this._scrollbackResolver = null;
        }
        break;
      }
      default:
        console.warn("unknown ws tag", tag);
    }
  }

  _onClose(ev) {
    if (this._onData) { this._onData.dispose(); this._onData = null; }
    if (this._stopped) return;

    // If the WS closed before any data arrived, treat as "session not
    // found / no longer exists" — show a toast and ask the dashboard for
    // a fresh session list. Do NOT auto-reconnect (the session is gone).
    if (!this._sawAnyMessage && ev.code === 1011) {
      showToast(`Session ${this.sessionId} no longer exists`);
      if (window.tmonks?.dashboard?.refresh) {
        window.tmonks.dashboard.refresh();
      }
      return;
    }

    // Normal close codes (user-initiated or server-shutdown): don't retry.
    if (ev.code === 1000 || ev.code === 1001) return;

    if (this._reconnectAttempt < RECONNECT_BACKOFFS_MS.length) {
      const delay = RECONNECT_BACKOFFS_MS[this._reconnectAttempt];
      showRetryStrip(`Reconnecting in ${delay}ms…`);
      this._reconnectAttempt += 1;
      setTimeout(() => {
        if (!this._stopped) this.connect();
      }, delay);
    } else {
      showRetryOverlay("tmux pane not reachable", () => {
        this._reconnectAttempt = 0;
        this.connect();
      });
    }
  }

  _onError(ev) {
    console.warn("ws error", ev);
  }
}

// ---- Terminal mount --------------------------------------------------------

/** VT byte sequences for the on-screen key buttons. */
const KEY_SEQUENCES = {
  esc: "\x1b",
  tab: "\t",
  "ctrl-c": "\x03",
  up: "\x1b[A",
  down: "\x1b[B",
  left: "\x1b[D",
  right: "\x1b[C",
};

function mountTerminal() {
  const container = $("#terminal-container");
  if (!container) return null;

  const term = new Terminal({
    allowProposedApi: true,
    scrollback: 10000,
    fontFamily: 'ui-monospace, "SF Mono", Menlo, monospace',
    fontSize: 13,
    convertEol: false,
    cursorBlink: true,
    theme: {
      background: "#000000",
      foreground: "#c9d1d9",
      cursor: "#58a6ff",
    },
  });

  const fit = new FitAddon();
  term.loadAddon(fit);
  term.loadAddon(new WebLinksAddon());
  const search = new SearchAddon();
  term.loadAddon(search);

  // Cmd-C / Ctrl-Shift-C with a selection → copy via clipboard API and
  // suppress the default ETX-on-Ctrl-C behavior. No selection → fall through
  // (so Ctrl-C still interrupts).
  // Cmd-F / Ctrl-F → toggle the search overlay; suppress browser default
  // (which would also open browser find).
  term.attachCustomKeyEventHandler((ev) => {
    if (ev.type !== "keydown") return true;
    const meta = ev.metaKey || ev.ctrlKey;
    if (meta && ev.key === "c" && term.hasSelection() && !ev.shiftKey) {
      const text = term.getSelection();
      if (text) {
        navigator.clipboard.writeText(text).catch(() => {});
        return false; // swallow — don't send ETX
      }
    }
    if (meta && (ev.key === "f" || ev.key === "F")) {
      toggleSearchOverlay(search, term);
      ev.preventDefault();
      return false;
    }
    return true;
  });

  term.open(container);
  fit.fit();

  // Hint the user; this is overwritten by the seed frame on session focus.
  term.writeln("tmonks \x1b[36m" + window.location.host + "\x1b[0m");
  term.writeln("\x1b[2mSelect a session in the sidebar to focus its pane.\x1b[0m");

  return { term, fit, search };
}

function toggleSearchOverlay(search, term) {
  const overlay = $("#search-overlay");
  if (!overlay) return;
  if (overlay.classList.contains("hidden")) {
    overlay.classList.remove("hidden");
    const input = $("#search-input");
    if (input) {
      input.value = "";
      input.focus();
    }
  } else {
    overlay.classList.add("hidden");
    search.clearDecorations();
    term.focus();
  }
}

function wireSearchOverlay(search, term) {
  const overlay = $("#search-overlay");
  const input = $("#search-input");
  const prev = $("#search-prev");
  const next = $("#search-next");
  const close = $("#search-close");
  const toggle = $("#search-toggle");
  if (!overlay || !input) return;

  toggle?.addEventListener("click", () => toggleSearchOverlay(search, term));

  const opts = { caseSensitive: false, wholeWord: false, regex: false };

  input.addEventListener("input", () => {
    if (input.value) search.findNext(input.value, opts);
  });
  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      if (ev.shiftKey) search.findPrevious(input.value, opts);
      else search.findNext(input.value, opts);
    }
    if (ev.key === "Escape") {
      overlay.classList.add("hidden");
      search.clearDecorations();
      term.focus();
    }
  });
  prev?.addEventListener("click", () => search.findPrevious(input.value, opts));
  next?.addEventListener("click", () => search.findNext(input.value, opts));
  close?.addEventListener("click", () => {
    overlay.classList.add("hidden");
    search.clearDecorations();
    term.focus();
  });
}

function wireKeyRow(term) {
  document.querySelectorAll("#key-row button[data-key]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const key = btn.dataset.key;
      const seq = KEY_SEQUENCES[key];
      if (seq) {
        term.input(seq, true); // wasUserInput=true → onData fires → forward to WS
        term.focus();
      }
    });
  });
}

function wirePasteButton(term) {
  $("#paste-clipboard")?.addEventListener("click", async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (text) {
        term.paste(text);
        term.focus();
      }
    } catch (e) {
      showToast(`Paste failed: ${e.message || e}`);
    }
  });
}

/** Strip ANSI escape sequences from raw bytes for plaintext copy. */
function stripAnsi(text) {
  // Lightweight: handle CSI, OSC (terminated by BEL or ESC\), and ESC X final-byte
  // forms. Doesn't need to be perfect — xterm.js's serialize-addon would be a
  // heavier alternative, but this is sufficient for the copy-scrollback use case.
  // eslint-disable-next-line no-control-regex
  return text
    .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "") // OSC
    .replace(/\x1b\[[?!>=]?[0-9;:]*[A-Za-z]/g, "")    // CSI
    .replace(/\x1b[ -\/]*[0-~]/g, "");                // ESC X
}

function wireCopyScrollback() {
  $("#copy-scrollback")?.addEventListener("click", async () => {
    const client = window.tmonks?.pane;
    if (!client) {
      showToast("No session focused");
      return;
    }
    try {
      const bytes = await client.requestScrollback();
      const text = new TextDecoder().decode(bytes);
      const stripped = stripAnsi(text);
      const sizeMb = (stripped.length / (1024 * 1024)).toFixed(2);
      if (stripped.length > 1024 * 1024 && isIOSSafari()) {
        if (!confirm(`Copy ${sizeMb} MiB of scrollback?`)) return;
      }
      await navigator.clipboard.writeText(stripped);
      showToast(`Copied ${sizeMb} MiB to clipboard`);
    } catch (e) {
      showToast(`Copy scrollback failed: ${e.message || e}`);
    }
  });
}

function isIOSSafari() {
  return /iPad|iPhone|iPod/.test(navigator.userAgent) && !/CriOS|FxiOS/.test(navigator.userAgent);
}

// ---- Sidebar toggle --------------------------------------------------------

function wireSidebarToggle() {
  const toggle = $("#sidebar-toggle");
  const sidebar = $("#sidebar");
  if (!toggle || !sidebar) return;
  toggle.addEventListener("click", () => {
    sidebar.classList.toggle("open");
  });
}

// ---- Retry UI --------------------------------------------------------------

function showRetryStrip(msg) {
  let strip = document.getElementById("retry-strip");
  if (!strip) {
    strip = document.createElement("div");
    strip.id = "retry-strip";
    strip.className = "retry-strip";
    document.body.appendChild(strip);
  }
  strip.textContent = msg;
  strip.style.display = "block";
}

function hideRetryStrip() {
  const strip = document.getElementById("retry-strip");
  if (strip) strip.style.display = "none";
}

function showRetryOverlay(message, onRetry) {
  let ov = document.getElementById("retry-overlay");
  if (!ov) {
    ov = document.createElement("div");
    ov.id = "retry-overlay";
    ov.className = "retry-overlay";
    document.body.appendChild(ov);
  }
  ov.innerHTML = "";
  const text = document.createElement("div");
  text.textContent = message;
  const btn = document.createElement("button");
  btn.textContent = "Retry";
  btn.addEventListener("click", () => {
    ov.style.display = "none";
    hideRetryStrip();
    onRetry();
  });
  ov.appendChild(text);
  ov.appendChild(btn);
  ov.style.display = "flex";
}

// ---- Toast helper ----------------------------------------------------------

function showToast(msg, ms = 4000) {
  let toast = document.getElementById("toast");
  if (!toast) {
    toast = document.createElement("div");
    toast.id = "toast";
    toast.className = "toast";
    document.body.appendChild(toast);
  }
  toast.textContent = msg;
  toast.style.display = "block";
  clearTimeout(toast._hideTimer);
  toast._hideTimer = setTimeout(() => {
    toast.style.display = "none";
  }, ms);
}

// ---- Bootstrap -------------------------------------------------------------

// ---- Dashboard WebSocket client --------------------------------------------

class DashboardClient {
  constructor(sidebar) {
    this.sidebar = sidebar;
    this.ws = null;
    this._reconnectAttempt = 0;
    this._stopped = false;
  }

  connect() {
    this._stopped = false;
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${proto}//${location.host}/ws/dashboard`;
    this.ws = new WebSocket(url);
    this.ws.addEventListener("open", () => {
      this._reconnectAttempt = 0;
      hideRetryStrip();
    });
    this.ws.addEventListener("message", (ev) => this._onMessage(ev));
    this.ws.addEventListener("close", (ev) => this._onClose(ev));
  }

  _onClose(ev) {
    if (this._stopped) return;
    if (ev.code === 1000 || ev.code === 1001) return;
    if (this._reconnectAttempt < RECONNECT_BACKOFFS_MS.length) {
      const delay = RECONNECT_BACKOFFS_MS[this._reconnectAttempt];
      this._reconnectAttempt += 1;
      setTimeout(() => { if (!this._stopped) this.connect(); }, delay);
    } else {
      showRetryOverlay("Dashboard not reachable", () => {
        this._reconnectAttempt = 0;
        this.connect();
      });
    }
  }

  /** Reconnect, forcing the server to send a fresh `sessions` frame. */
  refresh() {
    if (this.ws) {
      try { this.ws.close(); } catch (_) {}
    }
    this.connect();
  }

  _onMessage(ev) {
    if (typeof ev.data !== "string") return;
    let body;
    try { body = JSON.parse(ev.data); } catch (_) { return; }
    switch (body.type) {
      case "sessions": this.sidebar.renderSessions(body.items || []); break;
      case "status": this.sidebar.updateStatus(body.session_id, body.status, body.command); break;
      case "error": this.sidebar.markError(body.session_id, body.message); break;
      default: console.warn("unknown dashboard frame", body);
    }
  }
}

// ---- Sidebar component -----------------------------------------------------

class Sidebar {
  constructor(listEl, onSelect) {
    this.listEl = listEl;
    this.onSelect = onSelect;
    /** @type {Map<string, {row: HTMLElement, status: string, name: string, command: string}>} */
    this.items = new Map();
    this.activeId = null;
  }

  renderSessions(items) {
    if (!items.length) {
      this.items.clear();
      this.listEl.innerHTML = "";
      const empty = document.createElement("li");
      empty.className = "empty-hint empty-state";
      empty.textContent = "No tmux sessions found.";
      const code = document.createElement("code");
      code.textContent = "tmux new -s work";
      empty.appendChild(code);
      this.listEl.appendChild(empty);
      return;
    }

    // Set-diff merge: preserve scroll + focus state.
    const incoming = new Set(items.map((i) => i.id));
    for (const id of [...this.items.keys()]) {
      if (!incoming.has(id)) {
        const { row } = this.items.get(id);
        row.remove();
        this.items.delete(id);
      }
    }
    for (const { id, name } of items) {
      let entry = this.items.get(id);
      if (!entry) {
        entry = { row: this._buildRow(id, name), status: "unknown", name, command: "" };
        this.listEl.appendChild(entry.row);
        this.items.set(id, entry);
      } else if (entry.name !== name) {
        entry.row.querySelector(".session-name").textContent = name;
        entry.name = name;
      }
    }

    // Remove a stale empty-state if it's still around.
    const ghost = this.listEl.querySelector("li.empty-hint:not([data-id])");
    if (ghost) ghost.remove();
  }

  updateStatus(sessionId, status, command) {
    const entry = this.items.get(sessionId);
    if (!entry) return;
    const dot = entry.row.querySelector(".status-dot");
    dot.className = `status-dot ${status}`;
    const label = STATUS_LABELS[status] || status;
    const labelEl = entry.row.querySelector(".session-status");
    if (labelEl) labelEl.textContent = label;
    entry.row.setAttribute("aria-label", `${entry.name} — ${label}${command ? " (" + command + ")" : ""}`);
    if (command && command !== entry.command) {
      entry.command = command;
      const meta = entry.row.querySelector(".session-meta");
      if (meta) meta.textContent = command;
    }
    entry.status = status;
  }

  markError(sessionId, message) {
    const entry = this.items.get(sessionId);
    if (!entry) return;
    let err = entry.row.querySelector(".session-error");
    if (!err) {
      err = document.createElement("span");
      err.className = "session-error";
      err.textContent = "!";
      err.title = message;
      entry.row.appendChild(err);
    } else {
      err.title = message;
    }
  }

  setActive(sessionId) {
    if (this.activeId) {
      const prev = this.items.get(this.activeId);
      if (prev) prev.row.classList.remove("active");
    }
    this.activeId = sessionId;
    const entry = this.items.get(sessionId);
    if (entry) entry.row.classList.add("active");
  }

  _buildRow(id, name) {
    const li = document.createElement("li");
    li.dataset.id = id;
    li.setAttribute("role", "button");
    li.tabIndex = 0;
    li.setAttribute("aria-label", `${name} — unknown`);

    const dot = document.createElement("span");
    dot.className = "status-dot unknown";
    li.appendChild(dot);

    const meta = document.createElement("div");
    const nameEl = document.createElement("div");
    nameEl.className = "session-name";
    nameEl.textContent = name;
    meta.appendChild(nameEl);

    const cmdEl = document.createElement("div");
    cmdEl.className = "session-meta";
    meta.appendChild(cmdEl);

    li.appendChild(meta);

    const statusEl = document.createElement("span");
    statusEl.className = "session-status";
    statusEl.textContent = "unknown";
    li.appendChild(statusEl);

    li.addEventListener("click", () => this.onSelect(id));
    li.addEventListener("keypress", (ev) => {
      if (ev.key === "Enter" || ev.key === " ") {
        ev.preventDefault();
        this.onSelect(id);
      }
    });
    return li;
  }
}

// ---- Bootstrap -------------------------------------------------------------

document.addEventListener("DOMContentLoaded", () => {
  wireSidebarToggle();

  const mounted = mountTerminal();
  if (!mounted) return;

  wireSearchOverlay(mounted.search, mounted.term);
  wireKeyRow(mounted.term);
  wirePasteButton(mounted.term);
  wireCopyScrollback();

  window.tmonks = window.tmonks || {};
  window.tmonks.terminal = mounted;

  const openSession = (sessionId) => {
    if (window.tmonks.pane) {
      window.tmonks.pane.close();
    }
    const client = new PaneClient(mounted.term, mounted.fit, sessionId);
    client.connect();
    window.tmonks.pane = client;

    const container = $("#terminal-container");
    if (window.tmonks._paneRO) window.tmonks._paneRO.disconnect();
    const ro = new ResizeObserver(() => client.onContainerResize());
    ro.observe(container);
    window.tmonks._paneRO = ro;
    sidebar.setActive(sessionId);

    // Hide the sidebar drawer on mobile after selection.
    document.getElementById("sidebar")?.classList.remove("open");
    return client;
  };
  window.tmonks.openSession = openSession;
  window.tmonks.showToast = showToast;

  const sidebar = new Sidebar(document.getElementById("session-list"), openSession);
  const dashboard = new DashboardClient(sidebar);
  dashboard.connect();
  window.tmonks.dashboard = dashboard;
  window.tmonks.sidebar = sidebar;
});

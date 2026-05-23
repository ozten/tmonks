// tmons frontend — sidebar + focused-pane WebSocket client.
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

// ---- Pane WebSocket client --------------------------------------------------

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
  }

  connect() {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${proto}//${location.host}/ws/pane/${encodeURIComponent(this.sessionId)}`;
    this.ws = new WebSocket(url);
    this.ws.binaryType = "arraybuffer";
    this.ws.addEventListener("open", () => this._onOpen());
    this.ws.addEventListener("message", (ev) => this._onMessage(ev));
    this.ws.addEventListener("close", (ev) => this._onClose(ev));
    this.ws.addEventListener("error", (ev) => this._onError(ev));
  }

  close() {
    if (this._onData) {
      this._onData.dispose();
      this._onData = null;
    }
    if (this.ws && this.ws.readyState <= WebSocket.OPEN) {
      this.ws.close();
    }
    this.ws = null;
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
      const timer = setTimeout(() => {
        this.pendingScrollback = null;
        reject(new Error("scrollback request timed out"));
      }, 15000);
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
    // Unit 8 wires reconnect with exponential backoff here.
    if (ev.code !== 1000 && ev.code !== 1001) {
      showToast(`Pane closed (${ev.code}): ${ev.reason || "no reason"}`);
    }
  }

  _onError(ev) {
    console.warn("ws error", ev);
  }
}

// ---- Terminal mount --------------------------------------------------------

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

  term.open(container);
  fit.fit();

  // Hint the user; this is overwritten by the seed frame on session focus.
  term.writeln("tmons \x1b[36m" + window.location.host + "\x1b[0m");
  term.writeln("\x1b[2mSelect a session in the sidebar to focus its pane.\x1b[0m");

  return { term, fit, search };
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
  }

  connect() {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${proto}//${location.host}/ws/dashboard`;
    this.ws = new WebSocket(url);
    this.ws.addEventListener("message", (ev) => this._onMessage(ev));
    this.ws.addEventListener("close", () => {
      // Unit 8 wires reconnect logic.
    });
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
    entry.row.setAttribute("aria-label", `${entry.name} — ${status}${command ? " (" + command + ")" : ""}`);
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
    cmdEl.textContent = "";
    meta.appendChild(cmdEl);
    li.appendChild(meta);
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

  window.tmons = window.tmons || {};
  window.tmons.terminal = mounted;

  const openSession = (sessionId) => {
    if (window.tmons.pane) {
      window.tmons.pane.close();
    }
    const client = new PaneClient(mounted.term, mounted.fit, sessionId);
    client.connect();
    window.tmons.pane = client;

    const container = $("#terminal-container");
    if (window.tmons._paneRO) window.tmons._paneRO.disconnect();
    const ro = new ResizeObserver(() => client.onContainerResize());
    ro.observe(container);
    window.tmons._paneRO = ro;
    sidebar.setActive(sessionId);

    // Hide the sidebar drawer on mobile after selection.
    document.getElementById("sidebar")?.classList.remove("open");
    return client;
  };
  window.tmons.openSession = openSession;
  window.tmons.showToast = showToast;

  const sidebar = new Sidebar(document.getElementById("session-list"), openSession);
  const dashboard = new DashboardClient(sidebar);
  dashboard.connect();
  window.tmons.dashboard = dashboard;
  window.tmons.sidebar = sidebar;
});

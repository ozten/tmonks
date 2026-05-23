// tmons — minimal frontend shell (Unit 1).
// Subsequent units wire up the dashboard WS (Unit 5) and pane WS (Unit 4),
// the on-screen key row (Unit 7), and reconnect logic (Unit 8).
//
// CSP: this file is served `script-src 'self'`. No inline scripts anywhere.

import { Terminal } from "/assets/vendor/xterm.mjs";
import { FitAddon } from "/assets/vendor/addon-fit.mjs";
import { WebLinksAddon } from "/assets/vendor/addon-web-links.mjs";

const $ = (sel) => document.querySelector(sel);

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
  term.open(container);
  fit.fit();

  // ResizeObserver re-fits on container size changes. Resize WS frame
  // forwarding is added in Unit 8.
  const ro = new ResizeObserver(() => {
    try { fit.fit(); } catch (_) { /* xterm not ready */ }
  });
  ro.observe(container);

  // Greeting in lieu of a live pane (replaced when Unit 4 ships).
  term.writeln("tmons \x1b[36m" + window.location.host + "\x1b[0m");
  term.writeln("\x1b[2mNo session selected. Sessions appear in the sidebar when /ws/dashboard is wired (Unit 5).\x1b[0m");
  term.writeln("");

  return { term, fit };
}

function wireSidebarToggle() {
  const toggle = $("#sidebar-toggle");
  const sidebar = $("#sidebar");
  if (!toggle || !sidebar) return;
  toggle.addEventListener("click", () => {
    sidebar.classList.toggle("open");
  });
}

document.addEventListener("DOMContentLoaded", () => {
  wireSidebarToggle();
  window.tmons = window.tmons || {};
  window.tmons.terminal = mountTerminal();
});

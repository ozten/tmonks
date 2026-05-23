# tmons

Web UI into any tmux session. Because tmux is a nightmare and your prayers have been answered.

`tmons` is a single Rust binary that launches a local web server exposing a browser-based UI for your tmux sessions. Open the URL on a phone or laptop, see a sidebar of sessions with status badges (`idle` / `working` / `needs-input`), click one to focus it, and interact with the live session through `xterm.js` — including native browser copy/paste, on-screen keys for iOS-hostile inputs (`Esc` / `Tab` / `Ctrl-C` / arrows), and full physical-keyboard passthrough.

The MVP targets *agentic CLIs* specifically: `claude`, `codex`, and `opencode`. Sidebar badges reflect their live state — `working` while they're generating, `needs-input` when waiting on a tool-use confirmation, `idle-notify` when they're nudging you for input.

## Requirements

- Rust toolchain (stable, edition 2024 — `rustup default stable`)
- `tmux` ≥ 3.4 on `PATH`
- A modern browser (Chrome / Safari / Firefox; iOS Safari supported with the documented mobile caveats)

## Build

```bash
git clone <this repo>
cd tmonks
cargo build --release
```

The binary lands at `target/release/tmons`.

## Run

Start `tmons`. It binds to `127.0.0.1` on an ephemeral port and prints a one-time-token URL on stdout:

```bash
$ target/release/tmons
Open: http://127.0.0.1:54213/?t=Q9KFiL6F_d2Dz243KFcoVd16lvtYdAZBBuBwK4HVhdY
```

Open that URL in your browser. The first hit consumes the token, sets an `HttpOnly; SameSite=Strict` cookie, and redirects to `/`. From then on the browser drives the UI through the cookie — the token in the URL is the only way to obtain that cookie.

### CLI flags

| Flag | Default | Meaning |
|------|---------|---------|
| `--port <u16>` | `0` | TCP port; `0` picks one. |
| `--bind <ip>` | `127.0.0.1` | Loopback only. `0.0.0.0` is intentionally rejected — use SSH tunneling for remote access. |
| `--socket <name>` | (default) | tmux socket name (`-L <socket>`). Restricted to `[A-Za-z0-9_-]{1,32}`. |
| `--no-auth` | off | DANGEROUS. Requires `--i-understand-no-auth`. Skips token + cookie checks. |
| `--verbose` | off | Debug-level logging. |

### Remote access

`tmons` won't bind a non-loopback address. To use it on a remote machine:

```bash
# On your laptop:
ssh -L 8080:127.0.0.1:54213 remote-host

# Then open http://127.0.0.1:8080/?t=<token> in your local browser
# (the token still comes from the remote tmons stdout).
```

The Host-header allowlist and Origin check work naturally through the tunnel — `Host` and `Origin` both resolve to `127.0.0.1:8080`, so the upgrade succeeds.

### Using a non-default tmux socket

```bash
tmux -L work new-session -d -s api 'sleep 600'
tmons --socket work
```

Every tmux invocation inside `tmons` will prepend `-L work`.

## What you'll see

- **Sidebar**: one row per tmux session with a colored status dot, name, and the running command (e.g. `claude`, `bash`). Status badges update within ~1 s of state changes.
- **Main pane**: the active pane of the focused session, rendered via `xterm.js` with full ANSI color, spinners, and alt-screen TUIs.
- **Toolbar**: `Copy scrollback` (server-capped at 5 MiB), `Paste`, `Search` (Ctrl/Cmd-F).
- **Mobile**: at < 640 px the sidebar becomes a drawer behind a `☰` toggle, and a sticky key row at the bottom exposes `Esc`, `Tab`, `Ctrl-C`, and arrow keys.

## Security model

- Binds 127.0.0.1 by default. Token-in-URL + cookie-after-redirect; both the token and cookie are 32 random bytes (URL-safe base64). Cookie is `HttpOnly; SameSite=Strict; Path=/`.
- Host header must parse to a loopback IP (handles `127.0.0.1`, `localhost`, `[::1]`, `127.1`, decimal `2130706433`, hex, IPv4-mapped IPv6). DNS-rebinding via `nip.io` or evil.com is rejected with 403.
- Origin check on WebSocket upgrades: `Origin` host:port must exact-match the request `Host`. Defends against CSWSH from another local service on a different loopback port.
- CSP `default-src 'self'; connect-src 'self'; script-src 'self'; ...; frame-ancestors 'none'` on every response. No inline scripts; no external CDN.
- Outbound VT filter (tmux → browser) is an allowlist: drops DEC mouse modes, OSC 52 (clipboard hijack), OSC 8 hyperlinks with non-`http`/`https`/`mailto` schemes, DCS/APC/SOS/PM.
- Inbound VT filter (browser → tmux) drops the same risky categories and caps any inbound chunk at 4 KiB.
- Pane content is never logged, at any log level. `?t=<token>` is scrubbed from `tracing` spans.
- No telemetry, no auto-update, no remote endpoints contacted.

**What `tmons` does NOT defend against**: a compromised host, a malicious `~/.tmux.conf`, supply-chain attacks on dependencies, or anyone with read access to the printed URL (they get shell access). If you suspect the URL leaked — kill `tmons` and restart to rotate.

## Diagnostics

`GET /debug/state` (cookie-auth) returns build + runtime info as JSON:

```bash
$ curl -sS -b "tmons_session=$TOKEN" http://127.0.0.1:54213/debug/state
{"build":{"version":"0.1.0","commit":"unknown"},"bound_addr":"127.0.0.1:54213","no_auth":false,"socket":null}
```

Verbose logs to stderr with `--verbose` or `RUST_LOG=tmons=debug`.

## Known limitations (MVP)

- One focused pane at a time per browser tab. Switching sessions kills the old control-mode child and spawns a new one.
- Pane-tree / window-tree navigation, session creation/kill from the UI, splits, and saved snippets are roadmap, not v1.
- Two tabs on the same session interleave keystrokes — tmux handles multi-client cleanly at the protocol level, but typing in both at once will be confusing.
- `refresh-client -C` on resize affects all attached clients. If you have both a native tmux client and `tmons` attached to the same session, the pane will resize to whichever client is smallest. Consider `set-window-option aggressive-resize on` in `.tmux.conf`.
- Status detection is heuristic — marker text inside a quoted user message can produce false positives. Calibrated against current Claude Code 2.x / Codex 0.x / opencode 0.x; a version drift will surface a `WARN` at startup.

## Development

```bash
cargo test                 # 161 tests across 10 suites, ~2 s
cargo clippy --all-targets # lint
cargo run -- --verbose     # debug build with debug-level logs
```

Debug builds read assets from `assets/` on disk (instant frontend iteration). Release builds embed them via `rust-embed`.

## License

MIT OR Apache-2.0.

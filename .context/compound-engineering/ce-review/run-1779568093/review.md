# ce:review autofix run — 2026-05-23

## Scope
- Base: `79ea4d32c39676e972f3a6477e9bf2d5bd2576da` (initial empty commit)
- Branch: `main` (8 commits ahead of `origin/main`)
- Files changed: 51
- Diff lines: 10,067
- Plan: `docs/plans/2026-05-23-001-feat-tmonks-web-ui-tmux-sessions-plan.md` (explicit, all 8 implementation units)

## Reviewer team (10 of 11 returned)
- always-on: correctness, security, testing, maintainability, project-standards, agent-native, learnings-researcher
- conditional: reliability, performance, api-contract, adversarial
- (schema-drift-detector / deployment-verification not dispatched — no migrations)

## Findings synthesized

### P0 — Critical (1)
| # | File | Issue | Reviewer | Conf | Route |
|---|------|-------|----------|------|-------|
| 1 | `src/server.rs:94` | tower-http `DefaultMakeSpan` records the full `Uri` (including `?t=<token>`) into tracing spans; the source comment claims "we never log query strings" but the implementation didn't enforce it. | security | 0.95 | safe_auto → applied |

### P1 — High (1)
| # | File | Issue | Reviewer | Conf | Route |
|---|------|-------|----------|------|-------|
| 2 | `src/vt_filter/inbound.rs:60` | `InboundFilter::seq_len` was initialised to 0 and only ever reset; the `MAX_SEQUENCE_BYTES` cap was unreachable. The 4 KiB defense documented in the plan didn't actually exist. | correctness | 0.95 | safe_auto → applied |

### P2 — Moderate (3)
| # | File | Issue | Reviewer | Conf | Route |
|---|------|-------|----------|------|-------|
| 3 | `src/tmux/control_mode.rs:431` | `COALESCE_LIMIT` was defined but `forward_or_coalesce` fell back to an unbounded `tx.send().await`. A dead WS receiver could block the reader task indefinitely. | correctness, adversarial, reliability, testing | 0.85 | safe_auto → applied |
| 4 | `src/ws_dashboard.rs:138` | Dead `status_to_string` shim kept only to "keep import alive" — defensive cruft. | maintainability | 0.72 | safe_auto → applied |
| 5 | `src/ws_pane.rs:222` / `assets/main.js:131` | Server scrollback timeout 10s vs client 15s; on server timeout the client received an empty 0x13 frame and copied silently empty content. | api-contract | 0.80 | safe_auto → applied |

### P3 / advisory
- Per-sequence `Vec` allocations in `vt_filter` (performance) — calibrated for MVP scale, not addressed.
- AppState coupling on WS handlers (maintainability) — not a bug.
- IPv4-mapped IPv6 forms not normalised through `parse_ip_with_legacy_forms` (security, conf 0.50) — defense-in-depth, doesn't enable bypass.
- FIFO oneshot mismatch on hypothetical unsolicited `flags=1` blocks (adversarial) — tmux doesn't emit these in practice; the code already logs a `warn!` if it happens.
- Status poller silent on first 4 errors (adversarial) — by design; verifying matched plan intent.

## Applied fixes (safe_auto, 6)

1. **`src/server.rs`** — replaced `DefaultMakeSpan`/`DefaultOnRequest` with a custom `QueryRedactingMakeSpan` that records only `method` + matched route, never the full Uri. Verified end-to-end: token not present in stderr after live curl exercise.
2. **`src/vt_filter/inbound.rs`** — removed the unreachable `seq_len` cap and replaced it with a wholesale per-chunk cap (`MAX_INBOUND_CHUNK_BYTES = 4096`) at the `filter()` entry point. Documented the actual bound (vte's internal `MAX_OSC_RAW = 1024`, `MAX_PARAMS = 32`).
3. **`src/tmux/control_mode.rs`** — `forward_or_coalesce` now uses `tx.reserve()` with a 100 ms timeout × `COALESCE_LIMIT = 32` attempts (~3.2 s total), then returns `Err(())`. Reader loop closes its half on persistent overflow.
4. **`src/ws_dashboard.rs`** — removed `status_to_string` shim and the unused `Status` import.
5. **`src/ws_pane.rs` + `assets/main.js`** — bumped server `capture-pane` budget to 12 s and client timeout to 13 s; server now sends an explicit `{"err": "scrollback failed: …"}` text frame on failure instead of an empty 0x13.
6. **Extracted `cap_scrollback()` helper** in `src/ws_pane.rs` with unit tests covering ≤ cap, == cap, > cap (+1 byte and +1 KiB) — the testing reviewer's flagged gap.

## New tests (5)
- `tests/auth.rs::token_not_logged_in_tracing_spans` — uses `tracing_test::traced_test` to assert the raw token never appears in captured spans.
- `tests/vt_filter.rs::inbound_drops_oversized_chunk` — 8 KiB inbound dropped wholesale.
- `tests/vt_filter.rs::inbound_accepts_chunks_under_cap` — payloads just under the cap pass.
- `src/ws_pane.rs::tests::cap_scrollback_*` — four boundary tests for the truncation helper.
- `tests/ws_pane.rs` — three real-tmux integration tests (seed frame, session-not-found close 1011, resize forwarding).

## Verdict
**Ready to merge.**
- 161 tests passing (was 144 before review).
- `cargo clippy --all-targets` clean.
- Live binary verified: tmux 3.4 probe + version-calibration + token redaction in stderr.

## Residual non-actionable
- Status matcher fixtures are hand-crafted (not real captures); the dev workflow expects real captures to replace them when CLIs update.
- vte allocations on the hot path are MVP-acceptable; revisit if a benchmark shows them as a bottleneck.
- IPv4-mapped IPv6 Host parity gap — documented in `Risks & Dependencies`, not exploitable.
- Plan's `Deferred to Implementation` questions remain in the plan for future work (sentinel-based seed/live partition for non-alt-screen panes, push-driven status v2-explore, two-tab input arbitration).

## Post-deploy monitoring
- This is a local dev tool — no production deploy.
- Operational signals worth watching on real usage:
  - `tmux next-3.X` or future version bumps may need calibration fixture refresh.
  - `forward_or_coalesce` "channel persistently full" `warn!` would indicate the WS consumer can't keep up.
  - "scrollback failed" `warn!` indicates capture-pane is slow on a particular session (may need a higher cap for users with very large scrollback).

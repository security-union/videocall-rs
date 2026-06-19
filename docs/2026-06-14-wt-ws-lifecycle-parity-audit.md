# WebTransport ↔ WebSocket lifecycle parity audit (#509)

cc7tp follow-up (#503 / discussion #502). WebTransport is the production-default
transport (cc7tp: 8/8 participants on WT, 0 on WS), so a latent WT-only lifecycle
gap surfaces in production where a WS-only gap does not. This is a **verification
audit**, not a transport rewrite: it walks every row of the #509 lifecycle parity
matrix, cites the implementing code, and marks each row CLOSED or RESIDUAL-GAP.

Line numbers are anchors at the time of writing; the symbol names are the durable
reference.

## Parity matrix

| Event | WS path (file:line) | WT path (file:line) | Status |
|---|---|---|---|
| Initial connect | `WebSocketService::connect` — `videocall-transport/src/websocket.rs:113` | `WebTransportService::connect` / `connect_common` — `videocall-transport/src/webtransport.rs:592,745` | **CLOSED** — both construct, register open/close/error handlers, and surface a typed status enum. WT additionally classifies close **before** vs **after** `ready()` (`ClosedBeforeReady`/`ClosedAfterReady`, `webtransport.rs:511-513,790-800`), which is strictly richer than WS. |
| Reconnect with same `instance_id` | reconnection reuses the manager — `connection_manager.rs` reconnect path | URL carries a fresh `instance_id` per attempt (`append_instance_id`); server-reject-on-reuse fixed in #503 PR | **CLOSED** — verified post-#503. Each connection gets a fresh `conn_id` (`make_connection_id`) and instance id; no stale-id reuse on rejoin. |
| Graceful close | `Drop for WebSocketTask` → `ws.close()` if active — `websocket.rs:352-358` | `Drop for WebTransportTask` → `transport.close()` — `webtransport.rs:568-575` | **CLOSED (symmetric)** — both are Drop-only and emit a clean transport-level close. WS guards on `is_active()`; WT always calls `close()` (idempotent on the JS side once closing). See "Item #5" below. |
| Abrupt close (clean teardown) | TCP RST → browser `close`/`error` event → `WebSocketStatus::Closed/Error` | QUIC `CONNECTION_CLOSE` → `closed` promise → `ClosedAfterReady` | **CLOSED** — when the peer sends a real close (tab close, navigate away), both fire fast. |
| **Abrupt path-loss (NO clean teardown)** | TCP-level: server heartbeat `CLIENT_TIMEOUT` 30 s (`actix-api/src/constants.rs:26`); kernel RST may surface sooner | QUIC `max_idle_timeout` 30 s (`webtransport/mod.rs:39-42,215`) + app heartbeat `CLIENT_TIMEOUT` 30 s (`wt_chat_session.rs:543`, `WtInbound` resets `heartbeat` `:804`) | **RESIDUAL-GAP — documented, NOT fixed here.** See "Item #2" below. |
| Idle timeout | server app-heartbeat `CLIENT_TIMEOUT` 30 s, `HEARTBEAT_INTERVAL` 5 s (`constants.rs:22,26`) | QUIC `max_idle_timeout` 30 s + `keep_alive_interval` 1 s (`webtransport/mod.rs:44-47,215-222`) + app heartbeat 30 s / `WT_HEARTBEAT_INTERVAL` 5 s (`wt_chat_session.rs:57,543`) | **CLOSED** — app-level idle bound is the same 30 s on both. WT adds a transport-level 30 s QUIC backstop. |
| Backpressure detection (client) | WS send-buffer drop counter `websocket_drop_count()` → encoder self-shed (`camera_encoder.rs`, `screen_encoder.rs`) — PR #339 | WT unistream-drop `unistream_drop_count()` + slow-`ready()` `unistream_ready_stall_count()` → encoder self-shed via `evaluate_self_congestion` (`videocall-aq/src/constants.rs:1562`) | **CLOSED** — was the hypothesized #509 item #1 gap; landed in #1104 (drops) and #1219-prereq (saturation). See "Item #1" below; regression tests added by this audit. |
| Server-side handshake rejection | typed close info `(code, reason)` (`websocket.rs:64,192-206`); 1008 = expired JWT | `ClosedBeforeReady(msg)` (`webtransport.rs:511,800`) routed to `HandshakeFailed` | **CLOSED** — both surface a typed pre-session failure the manager classifies as `HandshakeFailed` and counts/recovers identically. |
| Pending-departure cancel on rejoin | `RECONNECT_GRACE_PERIOD` 3 s (`constants.rs:32`) cancels `PARTICIPANT_LEFT` if same user rejoins | same grace path (transport-agnostic, server-side) | **CLOSED** — grace window is in the session layer, not the transport, so WT/WS share it. Verified in cc7tp logs (#509). |
| Heartbeat / keepalive | client 5 s (`HEARTBEAT_KEEPALIVE_INTERVAL_MS` `videocall-aq/src/constants.rs:1256`); server timeout 30 s | same client 5 s heartbeat; server `WT_HEARTBEAT_INTERVAL` 5 s + QUIC `keep_alive_interval` 1 s | **CLOSED** — identical app-level cadence (5 s) and timeout (30 s). WT layers a 1 s QUIC PING under the app heartbeat. |
| Election (RTT testing) | RTT probes over the connection; candidate-rejection cascade fixed in #503 PR | same RTT-probe election; WT preferred within an RTT tier (`connection_manager.rs:2197,2216`) | **CLOSED** — verified post-#503. #522 hardened the probe pipeline against CPU stall; #572 escalates to a full reconnect after sustained suppression. |

## Item #1 — WT client-side backpressure (CLOSED; regression tests added)

WT backpressure is fully wired:

- Producer side increments two monotonic counters on the persistent-unistream
  media path: `unistream_drop_count()` (teardown / failed write,
  `webtransport.rs:65` reader, recorded via `record_unistream_drop`) and
  `unistream_ready_stall_count()` (slow-but-alive uplink saturation,
  `webtransport.rs:129` reader, recorded via `record_ready_stall`).
- Consumer side: both encoders read those counters once per AQ tick and feed
  them through the pure `evaluate_self_congestion` window/threshold helper
  (`videocall-aq/src/constants.rs:1562`) with the WT constants
  (`WT_SELF_CONGESTION_*`, `WT_SATURATION_*`), then `force_video_step_down()` —
  the WT analogue of PR #339's WS self-shed.

**Regression-coverage gap this audit closed.** The *decision* side was already
unit-tested (the `evaluate_self_congestion` tests). The *increment* side was
only half-covered: `record_ready_stall` had a host-testable seam, but the drop
counter's only write path was inline in the wasm-only `send_on_persistent_stream`
`spawn_local`, so no native test failed if it was deleted or pointed at the wrong
counter. And nothing pinned that each *encoder axis* feeds the **WT** constants
(not the WS or sibling-axis constants). This audit:

1. Extracted `record_unistream_drop()` (`videocall-transport/src/webtransport.rs`)
   as the single drop-counter write path, with native tests
   (`record_unistream_drop_increments_counter_once`,
   `record_unistream_drop_accumulates_across_repeated_drops`).
2. Extracted `wt_drop_step_down_decision` / `wt_saturation_step_down_decision`
   in both `camera_encoder.rs` and `screen_encoder.rs` (the wasm AQ loop now
   calls them), with native tests including an anti-misweave test that fails if
   an axis is fed the WS window/threshold instead of the WT one
   (`camera_wt_drop_axis_uses_wt_constants_not_ws`,
   `screen_wt_drop_axis_uses_wt_window_not_ws`).

All three were mutation-verified to fail when the wiring is broken.

## Item #2 — WT abrupt path-loss detection (RESIDUAL-GAP; documented, NOT fixed)

On a **true path-loss** — mobile leaving coverage, laptop sleep, NAT rebind —
where the client sends **no** QUIC `CONNECTION_CLOSE`, server-side WT detection
is bounded by two independent 30 s timers, neither faster than WS:

- **QUIC `max_idle_timeout` = 30 s** (`webtransport/mod.rs:39-42,215`). The QUIC
  idle timer resets on **any** received QUIC packet, including transport-level
  ACKs of the server's 1 s keep-alive PINGs (`keep_alive_interval = 1 s`,
  `:44-47,220`) and ACKs of still-in-flight media. So while the peer's OS still
  has a route and is ACKing, the 30 s idle timer keeps **resetting** and never
  fires.
- **App heartbeat `CLIENT_TIMEOUT` = 30 s** (`wt_chat_session.rs:543`). The
  server's `self.heartbeat` is reset **only** on inbound *application* data
  (`WtInbound` handler, `:804`) — NOT by QUIC keep-alive ACKs. So once real app
  traffic stops, this 30 s app-level backstop begins counting.

**Worst-case bound and the cc7tp discrepancy.** The two timers are not simply
additive (both are ~30 s and overlap), but the QUIC timer's *reset-on-any-packet*
behavior can defer transport-level death well past 30 s while ACKs flow, and only
*then* does the 30 s app heartbeat run to completion — so the effective detection
can approach **30 s (app) measured from when ACKs finally cease**, which itself
can be tens of seconds after the user physically dropped. The cc7tp orphan
lingered **~80 s** (`Unistream read error: ... session is closed` at
`15:13:11.288Z`), i.e. **80 s ≫ 30 s**. The leading hypothesis for the excess is
exactly the QUIC keep-alive interaction above: the server's 1 s PINGs (and/or
in-flight media drain) kept the orphan's QUIC connection technically alive — ACKs
still arriving — so neither the 30 s QUIC idle timer nor the app heartbeat (which
only counts true app silence) fired until the peer's link finally went fully
dark. WS, by contrast, can surface a kernel-level RST or a stalled-socket error
sooner on some paths.

**Why NOT fixed in this audit.** A faster-WT-dead-path detector (e.g. an
application-level ping/ack the server times independently of QUIC ACKs, or a
shorter `max_idle_timeout`) touches the real-time reconnect / re-election path on
every live connection and must be validated against high-latency (200 ms+), lossy,
and mobile links — exactly the HIGH-RISK class the Change Impact Policy guards. A
too-aggressive timeout would false-disconnect healthy slow links and trigger the
re-election cascades #503 just fixed. This belongs in its own focused change with
netsim validation, not a parity audit. A test that **pins the current bound** so a
future fix is measurable is documented as a follow-up (the relevant values —
`QUIC_MAX_IDLE_TIMEOUT_SECS` default 30, `CLIENT_TIMEOUT` 30 s — already have
sentinel coverage opportunities in `actix-api`, but a dedicated path-loss bound
test requires a live QUIC harness and is out of scope here).

## Item #3 — Production telemetry split by transport (item #4 in #509 body)

`CONNECTION_HANDSHAKE_FAILURES` / `CONNECTION_SESSION_DROPS` were global
`AtomicU64`s — a WS-heavy and a WT-heavy regression were indistinguishable in one
number, which defeats the audit's core "is WT ≫ WS?" question.

**Done (client-side only, no protobuf change):** the two counters are split per
transport into `_WT` / `_WS` statics with `record_handshake_failure(bool)` /
`record_session_drop(bool)` write paths (`connection_manager.rs`). The transport
is known statically at each `create_connection_lost_callback` call site (WS loop
passes `false`, WT loop passes `true`), mirroring the `is_webtransport` already
set on `ConnectionState::Connected`. Per-transport readers
(`connection_handshake_failures_wt/ws`, `connection_session_drops_wt/ws`) are
exported for local observability (perf panel / console).

**Deliberately NOT done (out of scope):** the values reported **over the wire**
stay the COMBINED totals — `connection_handshake_failures()` /
`connection_session_drops()` now return WT+WS sum and feed the **unchanged**
protobuf fields `connection_handshake_failures_total` /
`connection_session_drops_total`. Emitting the split to the relay would require
**new protobuf fields + a docker regen**, explicitly out of scope for this audit
(a prior protobuf-regen attempt in this batch caused churn). Wiring the split to
server-side telemetry / Grafana is the documented follow-up.

Tests: `handshake_failure_increments_only_the_matching_transport`,
`session_drop_increments_only_the_matching_transport`,
`combined_reader_equals_sum_of_both_transports` — mutation-verified (a swapped
WT/WS branch fails).

## Item #5 — Graceful close symmetry (CLOSED; not hardened)

`Drop for WebSocketTask` (`websocket.rs:352-358`) calls `ws.close()` guarded by
`is_active()`. `Drop for WebTransportTask` (`webtransport.rs:568-575`) calls
`transport.close()` unconditionally; the reader loops then break out of their
`reader.read()` awaits as the futures reject on the closed transport
(`webtransport.rs:628-636` and siblings). Both are Drop-only and emit a clean
transport-level close, so the server detects the gone session via its
channel-closed path (`WtChatSession::is_connection_dead`,
`wt_chat_session.rs:504-506`) — checked on the heartbeat **interval**
(`WT_HEARTBEAT_INTERVAL` = 5 s, `wt_chat_session.rs:533-540`), i.e. within ≤5 s of
the close, NOT after the 30 s heartbeat **timeout**. That is the parity #509 asked
to confirm: a clean close (WS or WT) tears down server-side within one heartbeat
interval rather than waiting out the full idle timeout. (Note: this fast path
requires a clean transport close — a true path-loss with no close is the Item #2
residual gap, bounded by the 30 s timers.) No hardening was applied: the WT
`close()` is already idempotent (a redundant `close()` on an
already-closing/closed session is a no-op on the JS side) and adding an
`is_active()`-style guard would require reading WT session state that the W3C API
does not expose as cleanly as WS `readyState`, for no behavioral gain.

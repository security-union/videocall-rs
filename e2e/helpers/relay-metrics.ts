/**
 * E2E helper: scrape the relay's Prometheus `/metrics` endpoint to OBSERVE the
 * viewport-aware video filter (HCL issue #988 / PR #994) actually dropping
 * off-screen VIDEO server-side.
 *
 * The relay increments a dedicated counter every time it INTENTIONALLY drops a
 * VIDEO packet because the source session is NOT in the receiver's VIEWPORT set
 * (see `RELAY_VIEWPORT_FILTERED_TOTAL` in `actix-api/src/metrics.rs` and the
 * `handle_msg` drop branch in `actix-api/src/actors/chat_server.rs`):
 *
 *   relay_viewport_filtered_total{room="<meeting_id>"}
 *
 * This is the ONLY authoritative SERVER-side signal that the relay actually
 * dropped viewport-filtered video. A DOM-only check (`data-off-budget="true"`)
 * proves the CLIENT shrank its decode set, but NOT that the relay acted on it.
 * Reading this counter is how a test tells "the relay is filtering" from "the
 * relay is quietly forwarding all video" — the distinction at the heart of the
 * issue-#988 feature and its issue-#995 regression coverage. Consumed by
 * `tests/viewport-reconnect-filter.spec.ts`.
 *
 * ## Why scrape BOTH relay processes
 *
 * The viewport drop-check lives in the transport-agnostic per-session NATS loop
 * (`chat_server.rs::handle_msg`), which runs inside BOTH relay binaries — the
 * WebSocket relay (`websocket_server`) and the WebTransport relay
 * (`webtransport_server`). Each is its own process with its OWN metrics
 * registry and its OWN `/metrics` HTTP endpoint, published to the host:
 *
 *   - WebSocket relay  : http://localhost:8080/metrics   (ACTIX_PORT=8080)
 *   - WebTransport relay: http://localhost:5321/metrics   (HEALTH_LISTEN_URL=...:5321)
 *
 * So a VIDEO packet forwarded over WS increments the counter in the :8080
 * process; one forwarded over WT increments it in the :5321 process. A test
 * therefore scrapes the endpoint matching the transport the RECEIVER elected.
 * (Routes registered in `actix-api/src/bin/{websocket,webtransport}_server.rs`.)
 *
 * These endpoints are part of the standard e2e stack (`make e2e-up`) — no
 * `impair`/toxiproxy profile is needed just to READ metrics. (A toxiproxy-driven
 * reconnect is a separate concern handled by the spec.)
 */

/** WebSocket relay `/metrics` endpoint (ACTIX_PORT=8080), published to the host. */
export const WS_RELAY_METRICS_URL =
  process.env.WS_RELAY_METRICS_URL || "http://localhost:8080/metrics";

/** WebTransport relay `/metrics` endpoint (HEALTH_LISTEN_URL :5321), published to the host. */
export const WT_RELAY_METRICS_URL =
  process.env.WT_RELAY_METRICS_URL || "http://localhost:5321/metrics";

/**
 * Fetch and return the raw Prometheus text exposition from a relay `/metrics`
 * endpoint. Throws with an actionable message if the endpoint is unreachable
 * (usually: the e2e stack is not up — `make e2e-up`).
 */
async function fetchMetricsText(url: string): Promise<string> {
  let res: Response;
  try {
    res = await fetch(url, { signal: AbortSignal.timeout(5_000) });
  } catch (e) {
    throw new Error(
      `relay /metrics unreachable at ${url} (${String(e)}). ` +
        "Is the e2e stack up? Bring it up with `make e2e-up` (or `make e2e-up-impair`).",
      { cause: e },
    );
  }
  if (!res.ok) {
    throw new Error(`relay /metrics returned HTTP ${res.status} from ${url}`);
  }
  return res.text();
}

/**
 * Parse a single `room`-labelled counter sample out of the Prometheus text
 * exposition and return its value for the given room, or 0 if the series is
 * absent (the relay only creates a `{room=...}` series the first time it
 * touches that room/branch — an absent series means "zero so far").
 *
 * Matches a line of the form:
 *   relay_viewport_filtered_total{room="my_room"} 42
 * Label order/extra labels are tolerated: we require the metric name at line
 * start and a `room="<room>"` label somewhere in the brace group. `room` is
 * regex-escaped because meeting ids are test-generated and otherwise safe, but
 * we never want a stray metachar to mis-match a different room's series.
 */
function parseRoomCounter(text: string, metric: string, room: string): number {
  const escapedRoom = room.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  // metric{...room="<room>"...} <value>
  const re = new RegExp(
    `^${metric}\\{[^}]*\\broom="${escapedRoom}"[^}]*\\}\\s+([0-9.eE+-]+)\\s*$`,
    "m",
  );
  const m = text.match(re);
  if (!m) return 0;
  const v = Number(m[1]);
  return Number.isFinite(v) ? v : 0;
}

/**
 * Read `relay_viewport_filtered_total{room=<room>}` — the cumulative count of
 * VIDEO packets the relay INTENTIONALLY dropped because their source was not in
 * a receiver's viewport set — from the relay serving `transport`.
 *
 * Returns 0 when the series does not exist yet (no drop has happened for this
 * room on this relay), which is the correct "nothing filtered yet" reading.
 *
 * @param transport Which relay process to scrape: "websocket" (:8080) or
 *                  "webtransport" (:5321). Must match the transport the
 *                  RECEIVER elected, since each process counts only its own
 *                  forwarded/dropped traffic.
 * @param room      The meeting id (the relay's `room` label === meeting_id).
 */
export async function readViewportFilteredTotal(
  transport: "websocket" | "webtransport",
  room: string,
): Promise<number> {
  const url = transport === "websocket" ? WS_RELAY_METRICS_URL : WT_RELAY_METRICS_URL;
  const text = await fetchMetricsText(url);
  return parseRoomCounter(text, "relay_viewport_filtered_total", room);
}

/**
 * Read `relay_viewport_forwarded_total{room=<room>}` — the denominator
 * complement: VIDEO packets that PASSED the viewport filter and were forwarded.
 * Used to prove the relay is actively making the filter DECISION for this room
 * (forwarded climbs whether or not anything is being dropped), so a flat
 * `filtered` counter can be distinguished from "no video reaching the filter
 * at all."
 */
export async function readViewportForwardedTotal(
  transport: "websocket" | "webtransport",
  room: string,
): Promise<number> {
  const url = transport === "websocket" ? WS_RELAY_METRICS_URL : WT_RELAY_METRICS_URL;
  const text = await fetchMetricsText(url);
  return parseRoomCounter(text, "relay_viewport_forwarded_total", room);
}

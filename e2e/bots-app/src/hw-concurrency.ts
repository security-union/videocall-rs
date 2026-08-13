// hw-concurrency.ts — resolve the optional `--hardware-concurrency` /
// `BOT_HW_CONCURRENCY` simulcast-layer-cap spoof value (issue #2035).
//
// Extracted from cli.ts so the parsing/validation contract is unit-testable
// without importing the CLI (which runs commander at import time).
//
// OPERATOR NOTES (pre-submit review):
//   - The cap only takes effect when the DEPLOYED client has simulcast ON
//     (`experimentalSimulcastMaxLayers >= 2`; default 3). If a deployment sets
//     it to 1, every bot encodes a single layer regardless of this value, and
//     the intended N-layer-per-bot load will NOT materialize — confirm the
//     target has simulcast enabled before relying on the cap.
//   - The spoof sets `navigator.hardwareConcurrency`, which ALSO drives the
//     bot's health-reported `client_cores` (peer telemetry / `cpu_throttled`),
//     not just the encode-layer ceiling. Bot telemetry reports this value, not
//     the node's real core count — expected for a load bot.
//   - CONSEQUENCE: do NOT read `videocall_client_cpu_throttled` for a spoofed
//     bot. It is `capability_score / cores < 150`, and `capability_score` is a
//     SINGLE-THREADED benchmark that does not scale with the spoof — so raising
//     the spoof lowers the ratio and makes the flag fire without any change in
//     real CPU. The ground truth for a starved pod is the container's CFS
//     throttling counter — which this tool does NOT sample; read
//     `container_cpu_cfs_throttled_seconds_total` from cluster Prometheus, not
//     the run CSVs — plus this tool's own fps-based RESOURCE_STARVED verdict
//     (resource/verdict.ts).

/**
 * Result of resolving a raw hardware-concurrency input.
 * - `ok` + `value: number`    → a strict positive integer; spoof at that count.
 * - `ok` + `value: undefined` → unset/empty OR `<= 0`; NO spoof (the browser's
 *   real `navigator.hardwareConcurrency` is used). `<= 0` is the documented
 *   disable sentinel — bot.ts only injects the spoof when the value is `> 0`.
 * - `invalid`                 → a malformed value; the caller should fail fast.
 */
export type HwConcurrencyResult =
  | { kind: "ok"; value: number | undefined }
  | { kind: "invalid"; message: string };

/**
 * Resolve `--hardware-concurrency` (or `BOT_HW_CONCURRENCY`) to the value passed
 * to `launchBot`'s `hardwareConcurrency`.
 *
 * Contract (kept in lockstep with the `--hardware-concurrency` CLI help and the
 * `BotRunOptions.hardwareConcurrency` doc in bot.ts):
 *   - unset / empty / whitespace  → `undefined` (no spoof)
 *   - a strict positive integer   → that integer (spoof enabled)
 *   - a value `<= 0`              → `undefined` (no spoof — the disable sentinel)
 *   - anything else               → invalid (must be rejected)
 *
 * The whole token is validated as an integer BEFORE `parseInt`, because
 * `Number.parseInt("6junk", 10)` silently returns `6` — accepting a numeric
 * prefix would launch with an unintended core count instead of failing fast as
 * the help promises. `"1.5"` and `"1e2"` are likewise rejected.
 */
export function resolveHardwareConcurrency(raw: string | undefined): HwConcurrencyResult {
  if (raw === undefined || raw.trim() === "") {
    return { kind: "ok", value: undefined };
  }
  const token = raw.trim();
  if (!/^-?\d+$/.test(token)) {
    return {
      kind: "invalid",
      message: `--hardware-concurrency (or BOT_HW_CONCURRENCY) must be an integer, got "${raw}"`,
    };
  }
  const parsed = Number.parseInt(token, 10);
  return { kind: "ok", value: parsed > 0 ? parsed : undefined };
}

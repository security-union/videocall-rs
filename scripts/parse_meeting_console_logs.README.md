# parse_meeting_console_logs.sh

Produces a fast (~10s) structured summary of a meeting's browser console logs: who joined, what transport they used, whether they had re-elections or connection failures, and what their machine looked like.

Use this **first**, before reaching for `grep` or `jq` on the raw log files, when investigating a meeting.

## Prerequisites

- `jq`, `zcat`, and GNU `date` on your PATH.
- A local directory of `*.log.gz` files pulled from the videocall-api pod's `/data/console-logs/<meeting>/<date>/` directory.

## Quick end-to-end workflow

```bash
# 1. Find the videocall-api pod
KUBECONFIG=~/vc-k3s-config.yaml
API_POD=$(kubectl get pod -l app.kubernetes.io/instance=videocall-api -n videocall \
          -o jsonpath='{.items[0].metadata.name}')

# 2. Pick a meeting + date (YYYY-MM-DD)
MEETING=infra
DATE=2026-05-06

# 3. Pull the logs locally
LOCAL_DIR="/tmp/console-logs/$MEETING/$DATE"
mkdir -p "$LOCAL_DIR"
kubectl exec "$API_POD" -n videocall -- \
  tar czf - -C /data/console-logs/$MEETING/$DATE . | tar xzf - -C "$LOCAL_DIR/"

# 4. Run the parser
./scripts/parse_meeting_console_logs.sh "$LOCAL_DIR"
```

## Modes

| Invocation | Purpose |
|---|---|
| `parse_meeting_console_logs.sh <log_dir>` | Markdown summary (default). Pipe to `less` or save to a file. |
| `parse_meeting_console_logs.sh <log_dir> --json` | Same data in JSON. Feed into other tools or jq queries. |
| `parse_meeting_console_logs.sh <log_dir> --verify` | Sanity check that every pattern the parser looks for still appears in the logs. Exits non-zero if a log message was renamed in client code and broke extraction silently. Use in CI or post-deploy spot-checks. |
| `parse_meeting_console_logs.sh <log_dir> --relay-wt=PATH` | Optionally ingest a videocall-webtransport relay pod log and add a **Slow-drain Receivers** section — joins server-side `Outbound channel full` drops to the peer-email map. Surfaces memory-pressured / slow clients (the Yu-Guo / RELAY-2 pattern from discussion #562). Can combine with default markdown or `--json`. |
| `parse_meeting_console_logs.sh -h` / `--help` | Show help summary. |

To pull the relay log: `kubectl logs -n videocall <videocall-webtransport-POD> --since=12h > /tmp/relay-wt.log`

## Sample output (markdown mode, trimmed)

```
## Meeting Log Summary: 2026-05-06

**Window:** 2026-05-06T13:03:46Z → 2026-05-06T13:18:35Z UTC
**Prometheus:** start=1778072340 end=1778075151

### Sessions

_Cores/Platform sourced from "level":"preamble" in first chunk. ⚠ flags clients likely to struggle in meetings ≥ 10 peers — see discussion #562._

| Email | Name | Start | Transport | RTT Base | Reelect | Chunks | Implaus RTT | Errors | End | Cores | Platform |
|-------|------|-------|-----------|----------|---------|--------|-------------|--------|-----|-------|----------|
| jason.gary@hcl-software.com | Jason Gary | 15:48:25 | websocket(ws_0) | 1072ms | 1 | 8 | 7 | 1 | **LOST** | 2 ⚠ | macOS 14.8.3 |
| kent.holtshouser@hcl-software.com | Kent | 15:49:49 | websocket(ws_0) | 101ms | 2 | 106 | 92 | 0 | ? | 6 ⚠ | macOS 15.3.1 |
| antonio.estrada@hcl-software.com | Tony Estrada | 15:01:01 | websocket(ws_0) | 73ms | 1 | 175 | 0 | 3 | clean | 12 | macOS 26.4.1 |
```

Also prints sections for: **Re-election Events**, **Implausible RTT Discards**, **Client Hardware Warnings**, **Concurrent Session Overlaps**, **Slow-drain Receivers** (when `--relay-wt=` is provided), **Peer ID → Email Map**, and a **Prometheus Copy-Paste** block with START/END epoch parameters pre-filled.

## Column reference

| Column | Meaning | What to look for |
|---|---|---|
| Start | Session start in UTC | — |
| Transport | WS or WT at election time | — |
| RTT Base | Baseline RTT at join, in ms | > 200ms = concerning. Compare to peers. |
| Reelect | Number of re-election triggers | > 0 = network instability during session |
| Chunks | Number of 30s log chunks uploaded | short sessions (< 3) often = tab closed before logging flushed |
| Implaus RTT | Number of RTT samples discarded as implausible | > 0 usually = client main-thread stall (not server clock drift). See discussion #562. |
| Speak | Count of `Speaking changed: false -> true` (VAD) | 0 = muted/listen-only; 100+ = active speaker. Helps distinguish "audio pipeline broken" from "person not talking". |
| Buf med | Median of non-zero NetEQ audio buffer depth, ms | 100–300ms = healthy; < 50ms = underrun risk (audible clicks); > 500ms = network jitter; zero-only samples filtered out (they represent peers not sending) |
| Errors | `level:error` log line count | categorize before alarming — one broken encoder can emit thousands of identical errors |
| End | `clean` if user left via UI, `LOST` if `Connection lost` event, `?` if neither | — |
| Cores | `navigator.hardwareConcurrency` from preamble | **< 6 ⚠** or **Intel Mac (macOS ≤ 15) with ≤ 8 cores ⚠** — see discussion #562 |
| Platform | OS + version from preamble | macOS 14 / 15 (pre-Apple-Silicon) often indicate old hardware |
| Concurrent | Count of overlapping sessions for same email (including 15s post-end NetEQ zombie window) | **> 1 ⚠** = duplicate NetEQ + AudioWorkletNode instances mixing into `master_gain` → audio crackling. See NETEQ-1 in discussion #562. |

## When to use `--verify`

Run `--verify` against a recent meeting's logs whenever you:

- Suspect the parser output is "thin" (lots of `?` or `unknown` rows that shouldn't be there)
- Land a client PR that touches `videocall-client/src/connection/*` or `dioxus-ui/src/components/attendants.rs`
- Want a spot-check that a deployment still emits every log line downstream tooling depends on

Required patterns (setup, election, preamble) must match or `--verify` exits 2. Optional patterns (re-elections, dropped datagrams) may legitimately be absent in a clean meeting.

If `--verify` fails, check the `PATTERN INVENTORY` block at the top of the script — each phrase is linked to the emitter file. A renamed log message needs both code and script updated in the same PR.

## Background + design notes

- Written 2026-05-05. Preamble columns + `--verify` mode added 2026-05-06. Concurrent-session detection + `--relay-wt` + speaking/buffer columns added 2026-05-08.
- Parser currently matches against free-text `msg` phrases from client code; this is fragile. Issue [#565](https://github01.hclpnp.com/labs-projects/videocall/issues/565) proposes adding a structured `event` field so parsers can key on stable event names instead.
- Full analysis context (hardware baseline for meeting sizes, JWT TTL bug, "implausible RTT ≠ clock drift" hypothesis, NetEQ duplication on transport switch, follow-up action items): [discussion #562](https://github01.hclpnp.com/labs-projects/videocall/discussions/562).

### Gotcha: stale Prometheus series look like active zombies

When a user session ends and another session starts, Prometheus series for the old `session_id` / `to_peer` can keep reporting the LAST known value for up to 5 minutes (the default scrape staleness). A `videocall_neteq_expand_ops_per_sec = 100/s` that appears "stuck" after a session change is often just frozen at its final scrape, NOT evidence of an active zombie NetEQ on the client.

To distinguish: check `videocall_neteq_packets_per_sec` for the same series. If packets are also frozen at a non-zero value with no variation over time, the series is stale. If packets are genuinely flowing (varying each scrape), the NetEQ is live. The `Concurrent` column in this script uses a 15-second NetEQ zombie window (matches `peer_decode_manager` heartbeat timeout), which is the realistic on-client lifetime — after that the NetEQ worker is terminated even though Prometheus may keep showing numbers.

## Performance

Typical runtime:
- 17-person 50-minute meeting (~2,100 chunks, ~2 GB gzipped): ~30 s
- 2-person 1-minute meeting (~5 chunks): < 1 s

Grep pre-filtering keeps jq's working set small. Parallelizing per-session has been tried and is not faster on current data (disk IO bound, not CPU).

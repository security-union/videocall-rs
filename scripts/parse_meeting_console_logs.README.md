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
| `parse_meeting_console_logs.sh <log_dir> --json` | The per-session table, peer map, meeting window and Prometheus params as JSON. Feed into other tools or jq queries. **Not** the analysis sections — Error Census, re-election events, implausible-RTT, hardware/capacity warnings, concurrent overlaps and simulcast changes are markdown-only. |
| `parse_meeting_console_logs.sh <log_dir> --verify` | Sanity check that every pattern the parser looks for still appears in the logs. Exits non-zero if a log message was renamed in client code and broke extraction silently. Use in CI or post-deploy spot-checks. |
| `parse_meeting_console_logs.sh <log_dir> --relay-wt=PATH` | Optionally ingest a videocall-webtransport relay pod log and add a **Slow-drain Receivers** section — joins server-side `Outbound channel full` drops to the peer-email map. Surfaces memory-pressured / slow clients (the Yu-Guo / RELAY-2 pattern from discussion #562). Can combine with default markdown or `--json`. |
| `parse_meeting_console_logs.sh <log_dir> --relay-ws=PATH` | Optionally ingest a videocall-**websocket** relay pod log and add a **WS Mailbox-Full Drops** section — joins server-side `Dropping inbound message ... (mailbox full)` drops to the peer-email map. This is the 16-slot actor-mailbox overflow that causes room-wide freezes (**issue #1057**); usually bursty fan-out storms (keyframe/join/screen-share spikes) that hit all receivers at once, including fast ones — NOT necessarily slow receivers. Prometheus equivalent: `relay_packet_drops_total{drop_reason="mailbox_full"}`. |
| `parse_meeting_console_logs.sh -h` / `--help` | Show help summary. |

To pull the relay logs: `kubectl logs -n videocall <videocall-webtransport-POD> --since=12h > /tmp/relay-wt.log` and `kubectl logs -n videocall <videocall-websocket-POD> --since=12h > /tmp/relay-ws.log`

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

Also prints sections for: **Error Census**, **Re-election Events**, **Implausible RTT Discards**, **Client Hardware Warnings**, **Concurrent Session Overlaps**, **Slow-drain Receivers** (when `--relay-wt=` is provided), **WS Mailbox-Full Drops** (when `--relay-ws=` is provided), **Peer ID → Email Map**, and a **Prometheus Copy-Paste** block with START/END epoch parameters pre-filled.

## Column reference

| Column | Meaning | What to look for |
|---|---|---|
| Start | Session start in UTC | — |
| Transport | WS or WT at election time | — |
| RTT Base | Baseline RTT at join, in ms | > 200ms = concerning. Compare to peers. |
| Reelect | Number of re-election triggers | > 0 = network instability during session |
| Chunks | Number of 30s log chunks uploaded | short sessions (< 3) often = tab closed before logging flushed |
| Implaus RTT | Number of RTT samples discarded as implausible | > 0 usually = client main-thread stall (not server clock drift). See discussion #562. |
| Speak | Count of `Speaking changed: false -> true` (VAD) | **Open-mic energy, not speech** — an RMS threshold on the raw mic stream, so it fires on breathing and background noise. Never read it as who was talking. Packet rate does not separate them either: a silent open mic transmits at the same ~50 pkt/s (#2278). For mute state use `videocall_self_audio_enabled`, written unconditionally from the sender's own report. |
| Buf med | Median of non-zero NetEQ audio buffer depth, ms | 100–300ms = healthy; < 50ms = underrun risk (audible clicks); > 500ms = network jitter; zero-only samples filtered out (they represent peers not sending) |
| Errors | `level:error` log line count | categorize before alarming — one broken encoder can emit thousands of identical errors. **This is a per-session count, so it HIDES any defect shared by several people — read the Error Census instead** (below). |
| End | `clean` if user left via UI, `LOST` if `Connection lost` event, `?` if neither | — |
| Cores | `navigator.hardwareConcurrency` from preamble | **< 6 ⚠** or **Intel Mac (macOS ≤ 15) with ≤ 8 cores ⚠** — see discussion #562 |
| Platform | OS + version from preamble | macOS 14 / 15 (pre-Apple-Silicon) often indicate old hardware |
| Concurrent | Count of overlapping sessions for same email (including 15s post-end NetEQ zombie window) | **> 1 ⚠** = duplicate NetEQ + AudioWorkletNode instances mixing into `master_gain` → audio crackling. See NETEQ-1 in discussion #562. |

## Error Census

Groups every `level:error` line by normalised message and ranks by **distinct
participants affected**, flagging each signature that hits more than one with ⚠.

```
### Error Census (grouped by message, ranked by participants affected)

| Count | People | Message |
|-------|--------|---------|
| 883   | 11 ⚠   | No connection manager available for Ok(MEDIA) packet |
| 12    | 9 ⚠    | panicked at …/host.rs:213:25: called `Result::unwrap()` on an `Err`… |
| 6     | 3 ⚠    | Microphone error: Failed to initialize audio encoder: JsValue(Invalid… |

⚠ **11 signature(s) affect MORE THAN ONE person — shared defects, not user oddities.**
```

**Why it exists.** The `Errors` column is a count, so a defect shared by several
people just inflates one row and reads as that person's problem. On the 2026-08-12
27-person call it found a `host.rs` panic across 9 participants and re-scoped an
`AudioWorkletNode` failure from 1 victim to 3.

**How to read it.** `People > 1` is the signal. Before filing anything, sweep the
signature so the issue carries the real victim count:

```bash
mlog <room>/<date> --who '<pattern>'
```

**Grouping rules.** Long ids (`[0-9]{6,}`) and per-occurrence durations collapse;
small integers are kept, because `401` vs `500` and close code `1006` vs `1000` are
different defects. Panics keep their second line, which carries the discriminating
reason. Signatures are truncated at 100 chars with a `…` marker. Stack frames are
dropped — they are continuations, not defects.

**Limits worth knowing.**
- The table is capped at 40 rows with a footer counting the remainder. A high row
  count usually means a message interpolates an id the normaliser does not
  collapse (server-supplied error bodies are the common case).
- Rows are never more than the error count — frames and empty messages are
  dropped by design, and identical messages collapse into one row. (Equal is
  normal: three errors with three distinct messages give three rows.)
- **Loss is detected, not inferred from emptiness, and the all-clear is withheld
  whenever either signal fires** — including when nothing survived to be counted.
  - *JSON layer (counted).* Under `jq -R` a malformed line does not abort the run;
    it yields no signature. Those, plus a jq that dies mid-stream, are counted and
    reported as `⚠ N error line(s) could not be parsed, or were lost …`, with the
    counts flagged as a **lower bound**.
  - *gzip layer (magnitude unknowable).* A truncated `.log.gz` decompresses only
    its prefix, so the missing lines never exist to be counted. Reported as
    `⚠ N session(s) had a truncated or corrupt .log.gz`. **Detection needs `zgrep`
    to exit ≥ 2 or print a gzip diagnostic; where it reports neither, this loss is
    undetected.**

## When to use `--verify`

Run `--verify` against a recent meeting's logs whenever you:

- Suspect the parser output is "thin" (lots of `?` or `unknown` rows that shouldn't be there)
- Land a client PR that touches `videocall-client/src/connection/*` or `dioxus-ui/src/components/attendants.rs`
- Want a spot-check that a deployment still emits every log line downstream tooling depends on

Required patterns (setup, election, preamble) must match or `--verify` exits 2. Optional patterns (re-elections, dropped datagrams) may legitimately be absent in a clean meeting.

If `--verify` fails, check the `PATTERN INVENTORY` block at the top of the script — each phrase is linked to the emitter file. A renamed log message needs both code and script updated in the same PR.

## Background + design notes

- Written 2026-05-05. Preamble columns + `--verify` mode added 2026-05-06. Concurrent-session detection + `--relay-wt` + speaking/buffer columns added 2026-05-08. `--relay-ws` (WS mailbox-full drops, issue #1057) + `network=` field-reliability caveat added 2026-06-03.
- Parser currently matches against free-text `msg` phrases from client code; this is fragile. Issue [#565](https://github01.hclpnp.com/labs-projects/videocall/issues/565) proposes adding a structured `event` field so parsers can key on stable event names instead.
- Full analysis context (hardware baseline for meeting sizes, JWT TTL bug, "implausible RTT ≠ clock drift" hypothesis, NetEQ duplication on transport switch, follow-up action items): [discussion #562](https://github01.hclpnp.com/labs-projects/videocall/discussions/562).

### Gotcha: stale Prometheus series look like active zombies

When a user session ends and another session starts, Prometheus series for the old `session_id` / `to_peer` can keep reporting the LAST known value for up to 5 minutes (the default scrape staleness). A `videocall_neteq_expand_ops_per_sec = 100/s` that appears "stuck" after a session change is often just frozen at its final scrape, NOT evidence of an active zombie NetEQ on the client.

To distinguish: check `videocall_neteq_packets_per_sec` for the same series. If packets are also frozen at a non-zero value with no variation over time, the series is stale. If packets are genuinely flowing (varying each scrape), the NetEQ is live. The `Concurrent` column in this script uses a 15-second NetEQ zombie window (matches `peer_decode_manager` heartbeat timeout), which is the realistic on-client lifetime — after that the NetEQ worker is terminated even though Prometheus may keep showing numbers.

## Performance

Typical runtime (measured 2026-08-12):
- 27-person 36-minute meeting (1,915 chunks, 40 MB gzipped / 355 MB raw): ~52 s
- 419 chunks: ~15 s
- 2-person 1-minute meeting (~5 chunks): < 1 s

The Error Census adds no decompression: it reuses the error lines Pass 3 already
materialises.

Grep pre-filtering keeps jq's working set small. Parallelizing per-session has been tried and is not faster on current data (disk IO bound, not CPU).

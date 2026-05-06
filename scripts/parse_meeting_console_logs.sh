#!/usr/bin/env bash
# Usage: parse_meeting_console_logs.sh <log_dir> [--json|--verify]
#        parse_meeting_console_logs.sh -h | --help
#
# Produces a structured summary of a pulled console-log directory.
# Uses grep pre-filtering for speed (15 sessions in ~5s vs 3+ minutes).
#
# For full docs including how to pull logs from the pod, column reference,
# and sample output, see: scripts/parse_meeting_console_logs.README.md
#
# Modes:
#   (default)       markdown summary to stdout
#   --json          machine-readable JSON to stdout
#   --verify        exit non-zero if any PATTERN INVENTORY phrase matched
#                   zero lines across the log dir. Use in CI or post-deploy
#                   checks to catch silent breakage when client code renames
#                   a log message.
#   -h | --help     show full usage and exit
#
# Dependencies: jq, zcat, date (GNU coreutils)
#
# ===========================================================================
# PATTERN INVENTORY
# ===========================================================================
# This script extracts signals from browser console logs that are shipped
# to /data/console-logs/<meeting>/<date>/*.log.gz. Every pattern below is
# a *free-text phrase* in the `msg` field — coupling between this parser
# and the client code is IMPLICIT and not type-checked.
#
# Rule: when you change any emitter below, UPDATE THIS SCRIPT IN THE SAME PR.
# Otherwise extraction silently stops working.
#
# Follow-up: issue #565 proposes adding a structured `event` field to each
# log entry so parsers can match on stable event names instead of phrases.
# Until that lands, maintain the inventory below by hand.
#
# | Phrase matched                              | Extracts         | Emitter (approximate)                                  |
# |---------------------------------------------|------------------|--------------------------------------------------------|
# | DIOXUS-UI: Creating VideoCallClient         | display_name    | dioxus-ui/src/components/attendants.rs                 |
# | Elected connection (ws_0|wt_0):             | transport_id   | videocall-client/src/connection/connection_manager.rs |
# | Baseline RTT for re-election monitoring: N  | rtt_baseline   | videocall-client/src/connection/connection_manager.rs |
# | Applying pending SESSION_ASSIGNED           | session_id     | videocall-client/src/connection/connection_manager.rs |
# | RTT degradation threshold reached           | reelection_times | videocall-client/src/connection/connection_manager.rs |
# | Discarding implausible RTT                  | implausible    | videocall-client/src/connection/connection_manager.rs |
# | Successfully left meeting                   | left_clean     | dioxus-ui/src/components/attendants.rs                 |
# | Connection lost / No valid connections      | connection_lost | videocall-client                                       |
# | datagram dropped                            | datagram_drops | videocall-client (WT transport)                        |
# | handshake failed / Opening handshake failed | handshake_failures | videocall-client (WT transport)                     |
# | "level":"preamble"                          | cores / memory / platform / etc. | videocall-client console-logger initialization |
# ===========================================================================

set -e

# -h / --help — print usage and exit 0
if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  cat <<EOF
Usage: $(basename "$0") <log_dir> [--json|--verify]
       $(basename "$0") -h | --help

Produces a structured summary of a pulled console-log directory.

Arguments:
  <log_dir>       Directory containing *.log.gz files pulled from the
                  videocall-api pod's /data/console-logs/<meeting>/<date>/.

Modes:
  (default)       Markdown summary to stdout.
  --json          Machine-readable JSON to stdout.
  --verify        Sanity-check that the log patterns this script depends
                  on still exist in the sample. Exits 2 if any REQUIRED
                  pattern (meeting setup, election, preamble) has zero
                  matches — this usually means a client-side log message
                  was renamed in code and broke extraction.
  -h, --help      Show this help.

Examples:
  # Summarize today's infra meeting
  $(basename "$0") /tmp/console-logs/infra/\$(date -u +%F)

  # Get structured JSON for scripting
  $(basename "$0") /tmp/console-logs/infra/\$(date -u +%F) --json | \\
    jq '.sessions[] | select(.preamble.underpowered == true)'

  # Verify parser matches the current client's log format
  $(basename "$0") /tmp/console-logs/infra/\$(date -u +%F) --verify

See scripts/parse_meeting_console_logs.README.md for the full workflow
(pulling logs from the pod, column reference, sample output).
EOF
  exit 0
fi

LOG_DIR="${1:-}"
OUTPUT_FORMAT="markdown"
case "${2:-}" in
  --json)   OUTPUT_FORMAT="json" ;;
  --verify) OUTPUT_FORMAT="verify" ;;
  "")       ;;  # default markdown
  *)        echo "Unknown option: $2" >&2; echo "Usage: $(basename "$0") <log_dir> [--json|--verify|-h]" >&2; exit 1 ;;
esac

if [[ -z "$LOG_DIR" || ! -d "$LOG_DIR" ]]; then
  echo "Usage: $(basename "$0") <log_dir> [--json|--verify|-h]" >&2
  exit 1
fi

ts_to_human() { date -u -d "@$(( ${1} / 1000 ))" '+%H:%M:%S' 2>/dev/null || echo "?"; }
epoch_to_prom() { echo "$(( ${1} / 1000 ))"; }

# ---------------------------------------------------------------------------
# Step 1: Enumerate unique (email, session_ts) pairs
# ---------------------------------------------------------------------------

declare -A SESSION_FILES_MAP  # key → space-separated sorted file list

while IFS= read -r f; do
  base=$(basename "$f")
  email="${base%%_*}"
  rest="${base#*_}"
  session_ts="${rest%%_*}"
  key="${email}::${session_ts}"
  SESSION_FILES_MAP["$key"]+="$f "
done < <(find "$LOG_DIR" -maxdepth 1 -name '*.log.gz' | sort)

declare -a ALL_KEYS
while IFS= read -r k; do ALL_KEYS+=("$k"); done < <(printf '%s\n' "${!SESSION_FILES_MAP[@]}" | sort)

# ---------------------------------------------------------------------------
# Step 2: Fast extraction — grep pre-filter + jq per session
# ---------------------------------------------------------------------------

TMPDIR_WORK=$(mktemp -d)
trap 'rm -rf "$TMPDIR_WORK"' EXIT

# Grep pattern covering all "interesting" log messages (key-event lines only)
KEY_EVENTS_GREP='DIOXUS-UI: Creating VideoCallClient|Elected connection |Baseline RTT for re-election|SESSION_ASSIGNED|RTT degradation threshold|Discarding implausible RTT|Successfully left meeting|Connection lost|No valid connections|datagram dropped|handshake failed|Opening handshake failed'

# Separate pattern for error-level lines
ERROR_GREP='"level":"error"'

for key in "${ALL_KEYS[@]}"; do
  email="${key%%::*}"
  session_ts="${key##*::}"

  # Sorted files for this session
  mapfile -t files < <(echo "${SESSION_FILES_MAP[$key]}" | tr ' ' '\n' | sort | grep -v '^$')
  chunk_count=${#files[@]}
  [[ $chunk_count -eq 0 ]] && continue

  # Pass 1: key event lines (fast grep pre-filter, then jq)
  key_json=$(zcat "${files[@]}" 2>/dev/null | \
    grep -E "$KEY_EVENTS_GREP" | \
    jq -sc '
      reduce .[] as $r (
        {dn:null, tid:null, rtt:null, sid:null, rtimes:[], left:null, clost:null, drops:0, hfail:0};
        if ($r.msg | startswith("DIOXUS-UI: Creating VideoCallClient")) then
          .dn = ($r.msg | capture("for (?<n>[^)]+) in ") | .n)
        elif ($r.msg | startswith("Elected connection ")) then
          if .tid == null then
            .tid = ($r.msg | split(":")[0] | ltrimstr("Elected connection "))
          else . end
        elif ($r.msg | startswith("Baseline RTT for re-election")) then
          if .rtt == null then
            .rtt = ($r.msg | capture("monitoring: (?<r>[0-9.]+)ms") | .r)
          else . end
        elif ($r.msg | test("Applying pending SESSION_ASSIGNED")) then
          if .sid == null then
            .sid = ($r.msg | capture("SESSION_ASSIGNED for [^:]+: (?<id>[0-9]{15,})") | .id)
          else . end
        elif ($r.msg | startswith("RTT degradation threshold reached")) then
          .rtimes += [$r.ts]
        elif ($r.msg | startswith("Successfully left meeting")) then
          .left = $r.ts
        elif ($r.msg | test("Connection lost|No valid connections")) then
          .clost = $r.ts
        elif ($r.msg | test("datagram dropped")) then
          .drops += 1
        elif ($r.msg | test("handshake failed|Opening handshake failed")) then
          .hfail += 1
        else . end
      )
    ' 2>/dev/null || echo '{}')

  # Pass 2: implausible RTT count (grep is sufficient — no jq needed)
  implausible=$(zcat "${files[@]}" 2>/dev/null | \
    grep -c "Discarding implausible RTT" 2>/dev/null || true)

  # Pass 3: error count
  error_count=$(zcat "${files[@]}" 2>/dev/null | \
    grep -c "$ERROR_GREP" 2>/dev/null || true)

  # Pass 4: preamble (client machine specs) — first chunk only, emits one
  # "level":"preamble" line near the top. Extract cores / memory / platform /
  # architecture / gpu. All fields are semicolon-delimited key=value pairs in
  # the `msg` string.
  preamble_msg=$(zcat "${files[0]}" 2>/dev/null | \
    grep -m1 '"level":"preamble"' | \
    jq -r '.msg' 2>/dev/null || echo "")
  pre_cores=$(echo "$preamble_msg" | grep -oE 'cores=[0-9]+' | head -1 | cut -d= -f2)
  pre_memory=$(echo "$preamble_msg" | grep -oE 'memory=[^;]+' | head -1 | cut -d= -f2-)
  pre_platform=$(echo "$preamble_msg" | grep -oE 'platform=[^;]+' | head -1 | cut -d= -f2-)
  pre_arch=$(echo "$preamble_msg" | grep -oE 'architecture=[^;]+' | head -1 | cut -d= -f2-)
  pre_gpu=$(echo "$preamble_msg" | grep -oE 'gpu=[^;]+' | head -1 | cut -d= -f2-)
  pre_screen=$(echo "$preamble_msg" | grep -oE 'screen=[^;]+' | head -1 | cut -d= -f2-)
  pre_app_version=$(echo "$preamble_msg" | grep -oE 'appVersion=[^;]+' | head -1 | cut -d= -f2-)

  # Flag underpowered client (discussion #562): cores < 6, or older Intel Mac
  # (macOS 14/15 AND cores <= 8). Emitted as simple bool so markdown can add ⚠.
  underpowered=false
  if [[ -n "$pre_cores" && "$pre_cores" -lt 6 ]]; then
    underpowered=true
  elif [[ "$pre_platform" == "macOS 14"* || "$pre_platform" == "macOS 15"* ]] \
       && [[ -n "$pre_cores" && "$pre_cores" -le 8 ]]; then
    underpowered=true
  fi

  # First/last timestamps from filename sort (filename IS the session_ts, last chunk = most recent)
  first_ts=$(zcat "${files[0]}" 2>/dev/null | jq -r '.ts' 2>/dev/null | head -1 || echo "")
  last_ts=$(zcat "${files[-1]}" 2>/dev/null | jq -r '.ts' 2>/dev/null | tail -1 || echo "")

  # Derive transport_type
  transport_id=$(echo "$key_json" | jq -r '.tid // "unknown"')
  if [[ "$transport_id" == wt_* ]]; then ttype="webtransport"
  elif [[ "$transport_id" == ws_* ]]; then ttype="websocket"
  else ttype="unknown"; fi

  echo "$key_json" | jq -c \
    --arg email "$email" \
    --arg session_ts "$session_ts" \
    --arg start_human "$(ts_to_human "$session_ts")" \
    --argjson chunk_count "$chunk_count" \
    --arg transport_type "$ttype" \
    --arg first_ts "$first_ts" \
    --arg last_ts "$last_ts" \
    --argjson implausible "$implausible" \
    --argjson error_count "$error_count" \
    --arg pre_cores "$pre_cores" \
    --arg pre_memory "$pre_memory" \
    --arg pre_platform "$pre_platform" \
    --arg pre_arch "$pre_arch" \
    --arg pre_gpu "$pre_gpu" \
    --arg pre_screen "$pre_screen" \
    --arg pre_app_version "$pre_app_version" \
    --argjson underpowered "$underpowered" \
    '{
      email: $email,
      session_ts: $session_ts,
      start_human: $start_human,
      chunk_count: $chunk_count,
      display_name: (.dn // "unknown"),
      transport_id: (.tid // "unknown"),
      transport_type: $transport_type,
      rtt_baseline: (.rtt // "?"),
      session_id: (.sid // ""),
      reelection_times: .rtimes,
      reelection_count: (.rtimes | length),
      left_clean: (.left // null),
      connection_lost: (.clost // null),
      datagram_drops: .drops,
      handshake_failures: .hfail,
      implausible_rtt_discards: $implausible,
      error_count: $error_count,
      first_ts: $first_ts,
      last_ts: $last_ts,
      preamble: {
        cores: $pre_cores,
        memory: $pre_memory,
        platform: $pre_platform,
        architecture: $pre_arch,
        gpu: $pre_gpu,
        screen: $pre_screen,
        app_version: $pre_app_version,
        underpowered: $underpowered
      }
    }' > "$TMPDIR_WORK/${key//[: @]/_}.json" 2>/dev/null || true
done

# ---------------------------------------------------------------------------
# Step 3: Meeting time range
# ---------------------------------------------------------------------------

all_session_ts=($(find "$LOG_DIR" -maxdepth 1 -name '*.log.gz' | \
  sed 's/.*_\([0-9]*\)_[0-9]*\.log\.gz$/\1/' | sort -n | uniq))

earliest_ms="${all_session_ts[0]:-0}"
latest_ms="${all_session_ts[-1]:-0}"
prom_start=$(epoch_to_prom "$earliest_ms")
prom_end=$(( $(epoch_to_prom "$latest_ms") + 1800 ))

first_log_file=$(find "$LOG_DIR" -maxdepth 1 -name '*.log.gz' | sort | head -1)
last_log_file=$(find "$LOG_DIR" -maxdepth 1 -name '*.log.gz' | sort | tail -1)
meeting_start=$(zcat "$first_log_file" 2>/dev/null | jq -r '.ts' 2>/dev/null | head -1 || echo "unknown")
meeting_end=$(zcat "$last_log_file" 2>/dev/null | jq -r '.ts' 2>/dev/null | tail -1 || echo "unknown")

# ---------------------------------------------------------------------------
# Step 4: Load all session records
# ---------------------------------------------------------------------------

mapfile -t session_jsons < <(
  ls "$TMPDIR_WORK/"*.json 2>/dev/null | sort | xargs -I{} cat {}
)

# ---------------------------------------------------------------------------
# Step 5: Output
# ---------------------------------------------------------------------------

meeting_id=$(basename "$LOG_DIR")

# --verify mode: each PATTERN INVENTORY phrase is checked against the log dir.
# "Required" patterns MUST match in any real meeting (meeting setup, elections,
# preamble). "Optional" patterns may legitimately be absent in small/clean
# meetings (no re-elections, no dropped datagrams). Required-with-zero-matches
# is an error and exits 2 — almost certainly a renamed client log message.
#
# Use in CI / pre-deploy checks: run after a client build to catch when a PR
# renames a log line and silently breaks extraction.
if [[ "$OUTPUT_FORMAT" == "verify" ]]; then
  # Patterns always expected in a real meeting (client setup + preamble)
  declare -a VERIFY_REQUIRED=(
    "DIOXUS-UI: Creating VideoCallClient"
    "Elected connection "
    "Baseline RTT for re-election monitoring"
    "Applying pending SESSION_ASSIGNED"
    '"level":"preamble"'
  )
  # Patterns we want to track but that are event-dependent (no failure in
  # zero-incident meetings; log for operator visibility only)
  declare -a VERIFY_OPTIONAL=(
    "RTT degradation threshold reached"
    "Discarding implausible RTT"
    "Successfully left meeting"
    "Connection lost"
    "datagram dropped"
    "handshake failed"
  )

  verify_failed=0
  echo "Pattern inventory check against $LOG_DIR:"

  # Use zcat + grep in a pipeline so we stream through the data instead of
  # buffering gigabytes into a bash variable. `grep -c` under `-r` on zcat
  # output won't work (grep -c counts lines from stdin, not per-file), so
  # pipe the whole stream through one grep per pattern.
  count_matches() {
    local pattern="$1"
    find "$LOG_DIR" -maxdepth 1 -name '*.log.gz' -print0 \
      | xargs -0 -r zcat 2>/dev/null \
      | grep -cF -- "$pattern" 2>/dev/null || true
  }

  echo "Required patterns:"
  for pattern in "${VERIFY_REQUIRED[@]}"; do
    count=$(count_matches "$pattern")
    if [[ "$count" -eq 0 ]]; then
      echo "  [FAIL]       0 matches: $pattern"
      verify_failed=1
    else
      printf '  [OK]   %7d matches: %s\n' "$count" "$pattern"
    fi
  done

  echo "Optional patterns (zero matches is OK if the meeting lacked those events):"
  for pattern in "${VERIFY_OPTIONAL[@]}"; do
    count=$(count_matches "$pattern")
    if [[ "$count" -eq 0 ]]; then
      echo "  [none]       0 matches: $pattern"
    else
      printf '  [OK]   %7d matches: %s\n' "$count" "$pattern"
    fi
  done

  if [[ "$verify_failed" -eq 1 ]]; then
    cat >&2 <<'EOF'

ERROR: one or more REQUIRED patterns had zero matches.

This likely means a client-side log message was renamed or removed in a
recent PR, and parse_meeting_console_logs.sh needs updating to match.
See the PATTERN INVENTORY block at the top of this script for each phrase's
emitter location.

If the sample is legitimately pre-election (e.g. a dir of lobby-only chunks),
re-run against a real meeting's logs.
EOF
    exit 2
  fi
  exit 0
fi

if [[ "$OUTPUT_FORMAT" == "json" ]]; then
  jq -n \
    --argjson sessions "$(printf '%s\n' "${session_jsons[@]}" | jq -s '.')" \
    --arg meeting_start "$meeting_start" \
    --arg meeting_end "$meeting_end" \
    --arg prom_start "$prom_start" \
    --arg prom_end "$prom_end" \
    --arg meeting_id "$meeting_id" \
    '{
      meeting_id: $meeting_id,
      meeting_start: $meeting_start,
      meeting_end: $meeting_end,
      prom_start_epoch: ($prom_start | tonumber),
      prom_end_epoch: ($prom_end | tonumber),
      sessions: $sessions,
      peer_map: [$sessions[] | select(.session_id != "") | {session_id, email, display_name}]
    }'
  exit 0
fi

# ---------------------------------------------------------------------------
# Markdown output
# ---------------------------------------------------------------------------

echo "## Meeting Log Summary: \`${meeting_id}\`"
echo ""
echo "**Window:** ${meeting_start} → ${meeting_end} UTC"
echo "**Prometheus:** start=\`${prom_start}\` end=\`${prom_end}\`"
echo ""

echo "### Sessions"
echo ""
echo "_Cores/Platform sourced from \`\"level\":\"preamble\"\` in first chunk. ⚠ flags clients likely to struggle in meetings ≥ 10 peers — see [discussion #562](https://github01.hclpnp.com/labs-projects/videocall/discussions/562)._"
echo ""
echo "| Email | Name | Start (UTC) | Transport | RTT Base | Reelect | Chunks | Implaus RTT | Errors | End | Cores | Platform |"
echo "|-------|------|-------------|-----------|----------|---------|--------|-------------|--------|-----|-------|----------|"

for s in "${session_jsons[@]}"; do
  email=$(echo "$s" | jq -r '.email')
  name=$(echo "$s" | jq -r '.display_name')
  start=$(echo "$s" | jq -r '.start_human')
  ttype=$(echo "$s" | jq -r '.transport_type')
  tid=$(echo "$s" | jq -r '.transport_id')
  rtt=$(echo "$s" | jq -r '.rtt_baseline')
  reelect=$(echo "$s" | jq -r '.reelection_count')
  chunks=$(echo "$s" | jq -r '.chunk_count')
  impl=$(echo "$s" | jq -r '.implausible_rtt_discards')
  errs=$(echo "$s" | jq -r '.error_count')
  left=$(echo "$s" | jq -r '.left_clean // ""')
  clost=$(echo "$s" | jq -r '.connection_lost // ""')
  if [[ -n "$left" && "$left" != "null" ]]; then end_status="clean"
  elif [[ -n "$clost" && "$clost" != "null" ]]; then end_status="**LOST**"
  else end_status="?"; fi
  # Preamble columns
  cores=$(echo "$s" | jq -r '.preamble.cores // ""')
  platform=$(echo "$s" | jq -r '.preamble.platform // ""')
  underpowered=$(echo "$s" | jq -r '.preamble.underpowered')
  [[ -z "$cores" ]] && cores="?"
  [[ -z "$platform" ]] && platform="?"
  flag=""
  [[ "$underpowered" == "true" ]] && flag=" ⚠"
  echo "| ${email} | ${name} | ${start} | ${ttype}(${tid}) | ${rtt}ms | ${reelect} | ${chunks} | ${impl} | ${errs} | ${end_status} | ${cores}${flag} | ${platform} |"
done

echo ""

echo "### Re-election Events"
echo ""
has_reelect=0
for s in "${session_jsons[@]}"; do
  reelect=$(echo "$s" | jq -r '.reelection_count')
  [[ "$reelect" -eq 0 ]] && continue
  has_reelect=1
  name=$(echo "$s" | jq -r '.display_name')
  email=$(echo "$s" | jq -r '.email')
  start=$(echo "$s" | jq -r '.start_human')
  times=$(echo "$s" | jq -r '.reelection_times | join(", ")')
  lost=$(echo "$s" | jq -r '.connection_lost // "none"')
  echo "**${name} (${email}) session @${start}:** ${reelect} trigger(s)"
  echo "- Times: ${times}"
  echo "- Connection lost at: ${lost}"
  echo ""
done
[[ $has_reelect -eq 0 ]] && echo "_None._" && echo ""

echo "### Implausible RTT Discards (main-thread stall or server clock drift)"
echo ""
echo "_Per discussion #562: these are more often main-thread stalls on underpowered clients than server clock drift. Cross-check the Cores column in the Sessions table._"
echo ""
has_impl=0
for s in "${session_jsons[@]}"; do
  impl=$(echo "$s" | jq -r '.implausible_rtt_discards')
  [[ "$impl" -eq 0 ]] && continue
  has_impl=1
  name=$(echo "$s" | jq -r '.display_name')
  email=$(echo "$s" | jq -r '.email')
  start=$(echo "$s" | jq -r '.start_human')
  ttype=$(echo "$s" | jq -r '.transport_type')
  cores=$(echo "$s" | jq -r '.preamble.cores // "?"')
  platform=$(echo "$s" | jq -r '.preamble.platform // "?"')
  echo "- **${name} (${email}) @${start} [${ttype}]: ${impl} discards** (cores=${cores}, ${platform}) — re-election watchdog blind"
done
[[ $has_impl -eq 0 ]] && echo "_None._"
echo ""

echo "### Client Hardware Warnings"
echo ""
echo "_Flagged by preamble heuristics (cores < 6, or Intel Mac on macOS 14/15 with cores ≤ 8). Deduplicated per email._"
echo ""
declare -A SEEN_UW
has_uw=0
for s in "${session_jsons[@]}"; do
  underpowered=$(echo "$s" | jq -r '.preamble.underpowered')
  [[ "$underpowered" != "true" ]] && continue
  email=$(echo "$s" | jq -r '.email')
  [[ -n "${SEEN_UW[$email]:-}" ]] && continue
  SEEN_UW[$email]=1
  has_uw=1
  name=$(echo "$s" | jq -r '.display_name')
  cores=$(echo "$s" | jq -r '.preamble.cores // "?"')
  memory=$(echo "$s" | jq -r '.preamble.memory // "?"')
  platform=$(echo "$s" | jq -r '.preamble.platform // "?"')
  echo "- **${name} (${email})**: cores=${cores}, memory=${memory}, platform=${platform}"
done
[[ $has_uw -eq 0 ]] && echo "_None._"
echo ""

echo "### Peer ID → Email Map (for Prometheus)"
echo ""
echo "| Session ID (uint64) | Email | Display Name |"
echo "|---------------------|-------|--------------|"
for s in "${session_jsons[@]}"; do
  sid=$(echo "$s" | jq -r '.session_id')
  [[ -z "$sid" || "$sid" == "null" || "$sid" == "" ]] && continue
  email=$(echo "$s" | jq -r '.email')
  name=$(echo "$s" | jq -r '.display_name')
  echo "| \`${sid}\` | ${email} | ${name} |"
done

echo ""
echo "### Prometheus Copy-Paste"
echo ""
echo "\`\`\`bash"
echo "PROM=https://prometheus.videocall.fnxlabs.com"
echo "MEETING_ID=${meeting_id}"
echo "START=${prom_start}"
echo "END=${prom_end}"
echo ""
echo "# Call quality scores:"
echo "curl -sk \"\$PROM/api/v1/query_range\" \\"
echo "  --data-urlencode \"query=videocall_call_quality_score{meeting_id=\\\"\$MEETING_ID\\\"}\" \\"
echo "  --data-urlencode \"start=\$START\" --data-urlencode \"end=\$END\" --data-urlencode \"step=15s\""
echo ""
echo "# Audio concealment:"
echo "curl -sk \"\$PROM/api/v1/query_range\" \\"
echo "  --data-urlencode \"query=videocall_audio_concealment_pct{meeting_id=\\\"\$MEETING_ID\\\"}\" \\"
echo "  --data-urlencode \"start=\$START\" --data-urlencode \"end=\$END\" --data-urlencode \"step=15s\""
echo "\`\`\`"

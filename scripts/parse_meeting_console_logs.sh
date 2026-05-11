#!/usr/bin/env bash
# Usage: parse_meeting_console_logs.sh <log_dir> [--json|--verify] [--relay-wt=PATH]
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
#   --relay-wt=P    optional: path to a videocall-webtransport relay pod
#                   log file. When provided, emits a "Slow-drain Receivers"
#                   section joining "Outbound channel full" drops per
#                   session to the peer map from console logs. Useful for
#                   identifying memory-pressured / slow clients (see
#                   discussion #562, RELAY-2 pattern).
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
# CONSOLE-LOG patterns:
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
# | Speaking changed: false -> true             | speaking_transitions (VAD proxy for "actually spoke") | videocall-client mic/VAD        |
# | audio health (buffer: Nms) for peer: X      | audio_buffer_median_ms per peer   | videocall-client/src/health_reporter.rs |
# | "level":"preamble"                          | cores / memory / platform / etc. | videocall-client console-logger initialization |
#
# RELAY-POD patterns (when --relay-wt=PATH is provided):
# | Phrase matched                              | Extracts                         | Emitter                                         |
# |---------------------------------------------|----------------------------------|-------------------------------------------------|
# | Outbound channel full for session <ID>      | drops_per_session (slow-drain)  | actix-api/src/actors/transports/wt_chat_session.rs |
# ===========================================================================

set -e

# -h / --help — print usage and exit 0
if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  cat <<EOF
Usage: $(basename "$0") <log_dir> [--json|--verify] [--relay-wt=PATH]
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
  --relay-wt=P    Optional path to a videocall-webtransport relay pod
                  log file (plain text, kubectl logs output). When
                  provided, emits a "Slow-drain Receivers" section
                  showing per-session "Outbound channel full" drop
                  counts joined to the peer map. Useful for spotting
                  memory-pressured / slow clients (RELAY-2 pattern,
                  see discussion #562).
  -h, --help      Show this help.

Examples:
  # Summarize today's infra meeting
  $(basename "$0") /tmp/console-logs/infra/\$(date -u +%F)

  # Get structured JSON for scripting
  $(basename "$0") /tmp/console-logs/infra/\$(date -u +%F) --json | \\
    jq '.sessions[] | select(.preamble.underpowered == true)'

  # Verify parser matches the current client's log format
  $(basename "$0") /tmp/console-logs/infra/\$(date -u +%F) --verify

  # Cross-reference with relay pod backpressure
  $(basename "$0") /tmp/console-logs/infra/\$(date -u +%F) \\
    --relay-wt=/tmp/relay-webtransport.log

See scripts/parse_meeting_console_logs.README.md for the full workflow
(pulling logs from the pod, column reference, sample output).
EOF
  exit 0
fi

LOG_DIR="${1:-}"
OUTPUT_FORMAT="markdown"
RELAY_WT=""
# Parse remaining args: $2..$N. --json / --verify set format; --relay-wt=PATH sets relay path.
shift  # drop $1 (LOG_DIR)
for arg in "$@"; do
  case "$arg" in
    --json)
      if [[ "$OUTPUT_FORMAT" == "verify" ]]; then
        echo "Error: --json and --verify are mutually exclusive" >&2; exit 1
      fi
      OUTPUT_FORMAT="json" ;;
    --verify)
      if [[ "$OUTPUT_FORMAT" == "json" ]]; then
        echo "Error: --json and --verify are mutually exclusive" >&2; exit 1
      fi
      OUTPUT_FORMAT="verify" ;;
    --relay-wt=*)    RELAY_WT="${arg#--relay-wt=}" ;;
    "")              ;;  # skip empty
    *)               echo "Unknown option: $arg" >&2
                     echo "Usage: $(basename "$0") <log_dir> [--json|--verify] [--relay-wt=PATH]" >&2
                     exit 1 ;;
  esac
done

if [[ -z "$LOG_DIR" || ! -d "$LOG_DIR" ]]; then
  echo "Usage: $(basename "$0") <log_dir> [--json|--verify] [--relay-wt=PATH]" >&2
  exit 1
fi

if [[ -n "$RELAY_WT" && ! -f "$RELAY_WT" ]]; then
  echo "--relay-wt: file not found: $RELAY_WT" >&2
  exit 1
fi

if ! command -v gawk &>/dev/null; then
  echo "Error: this script requires gawk (asort is a gawk extension)" >&2
  echo "       Install gawk and re-run; otherwise buffer medians silently degrade." >&2
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

  # Pass 3b: speaking_transitions — count VAD false->true transitions.
  # A good proxy for "did the user actually speak?" Low/zero means
  # muted or listen-only; high (100+) means active speaker. Useful
  # when triaging audio complaints: listeners with 0 transitions
  # aren't contributing to the audio mix.
  speaking_transitions=$(zcat "${files[@]}" 2>/dev/null | \
    grep -cF "Speaking changed: false -> true" 2>/dev/null || true)

  # Pass 3c: audio_buffer stats — NetEQ-reported per-peer buffer depth
  # extracted from "audio health (buffer: Nms) for peer: X". Summarize
  # n_samples, median (incl. zeros), median_nonzero (only samples >0ms),
  # and n_nonzero (i.e., times this session was actually receiving audio
  # from someone). median_nonzero is the useful crackling signal —
  # medians-including-zero are dominated by muted peers reporting 0ms.
  audio_buffer_stats=$(zcat "${files[@]}" 2>/dev/null | \
    grep -oE 'audio health \(buffer: [0-9]+ms\)' | \
    grep -oE '[0-9]+' | \
    awk 'BEGIN {n=0; nz=0} {
      a[n++]=$1
      if ($1 > 0) b[nz++]=$1
    } END {
      if (n == 0) { print "{\"n\":0,\"median_ms\":null,\"n_nonzero\":0,\"median_nonzero_ms\":null}"; exit }
      asort(a)
      median = a[int(n/2)+1]
      if (nz == 0) {
        printf "{\"n\":%d,\"median_ms\":%d,\"n_nonzero\":0,\"median_nonzero_ms\":null}\n", n, median
      } else {
        asort(b)
        median_nz = b[int(nz/2)+1]
        printf "{\"n\":%d,\"median_ms\":%d,\"n_nonzero\":%d,\"median_nonzero_ms\":%d}\n", n, median, nz, median_nz
      }
    }' 2>/dev/null || echo '{"n":0,"median_ms":null,"n_nonzero":0,"median_nonzero_ms":null}')

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
    --argjson speaking_transitions "$speaking_transitions" \
    --argjson audio_buffer "$audio_buffer_stats" \
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
      speaking_transitions: $speaking_transitions,
      audio_buffer: $audio_buffer,
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
# Step 4b: Concurrent-session overlap detection
# ---------------------------------------------------------------------------
# Two sessions belonging to the SAME email are "concurrent" if their active
# windows overlap. Flag >1 = duplicate NetEQ / AudioWorkletNode risk on the
# client (NETEQ-1 in discussion #562). Populates a map: session_ts → count
# of overlapping sessions from the same email (including self).

declare -A CONCURRENT_MAP  # "${email}::${session_ts}" → count
if [[ ${#session_jsons[@]} -gt 0 ]]; then
  # Dump all sessions into one JSON array then compute overlaps in jq.
  all_sessions_json=$(printf '%s\n' "${session_jsons[@]}" | jq -s '.')
  while IFS=$'\t' read -r key count; do
    CONCURRENT_MAP["$key"]="$count"
  done < <(echo "$all_sessions_json" | jq -r '
    # Use session_ts (epoch ms from the filename) as start_ms — reliable
    # session-start anchor. last_ts (last log ISO timestamp) is the end;
    # strip ".NNN" fractional seconds before fromdateiso8601 (which does
    # not accept them). A session log chunk may contain prior-page
    # entries that predate session_ts, so first_ts is unreliable for
    # this purpose and intentionally ignored.
    def to_ms_iso:
      if . == null or . == "" then null
      else (. | sub("\\.[0-9]+Z$"; "Z")) | (fromdateiso8601 * 1000)
      end;

    # Pad window by CLIENT_NETEQ_LIFETIME_MS after last_ts: peer_decode_manager
    # keeps the old Peer (and its NetEqAudioPeerDecoder + AudioWorkletNode)
    # alive for up to 3 missed 5s heartbeats = 15s after last activity.
    # During this window the old NetEQ is still mixing into master_gain,
    # so for NETEQ-1 detection we extend the effective "end" by 15s.
    15000 as $neteq_zombie_ms |

    map(
      . as $s |
      ($s.last_ts | to_ms_iso) as $last_iso_ms |
      ($s.session_ts | tonumber) as $start_ms |
      (if $last_iso_ms == null then $start_ms else $last_iso_ms end) as $raw_end_ms |
      ([$raw_end_ms, $start_ms] | max) as $clamped_end_ms |
      {
        email: $s.email,
        session_ts: $s.session_ts,
        start_ms: $start_ms,
        end_ms: ($clamped_end_ms + $neteq_zombie_ms)
      }
    ) as $all |

    # For each session, count peers with same email whose window overlaps
    # (inclusive on both ends). Result includes self (min count = 1).
    $all[] |
    . as $me |
    [$all[] | select(.email == $me.email) |
      select(
        ($me.start_ms != null and $me.end_ms != null
         and .start_ms != null and .end_ms != null
         and $me.start_ms <= .end_ms
         and .start_ms <= $me.end_ms)
      )] | length as $count |
    "\($me.email)::\($me.session_ts)\t\($count)"
  ')
fi

# ---------------------------------------------------------------------------
# Step 4c: Relay-WT log ingest (--relay-wt=PATH)
# ---------------------------------------------------------------------------
# Count "Outbound channel full for session <id>" drops per session.
# Filter to session_ids present in this meeting's console logs so noise
# from other meetings in the same relay log is excluded.

declare -A RELAY_DROPS  # session_id (uint64 string) → drop count
RELAY_DROPS_TOTAL=0
if [[ -n "$RELAY_WT" ]]; then
  # Build set of in-meeting session_ids.
  declare -A IN_MEETING_SIDS
  for s in "${session_jsons[@]}"; do
    sid=$(echo "$s" | jq -r '.session_id')
    [[ -n "$sid" && "$sid" != "null" ]] && IN_MEETING_SIDS["$sid"]=1
  done

  # Parse drop lines. Format:
  #   ERROR ... Outbound channel full for session 1311..., dropping message
  while read -r count sid; do
    [[ -z "$sid" ]] && continue
    if [[ -n "${IN_MEETING_SIDS[$sid]:-}" ]]; then
      RELAY_DROPS["$sid"]="$count"
      RELAY_DROPS_TOTAL=$((RELAY_DROPS_TOTAL + count))
    fi
  done < <(grep -oE 'Outbound channel full for session [0-9]+' "$RELAY_WT" 2>/dev/null \
           | awk '{print $NF}' \
           | sort \
           | uniq -c \
           | awk '{print $1, $2}')
fi

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
    "Speaking changed: false -> true"
    "audio health (buffer:"
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
echo "_Cores/Platform sourced from \`\"level\":\"preamble\"\` in first chunk. ⚠ flags clients likely to struggle in meetings ≥ 10 peers (underpowered) or with concurrent duplicate sessions (NetEQ duplication — NETEQ-1) — see [discussion #562](https://github01.hclpnp.com/labs-projects/videocall/discussions/562)._"
echo ""
echo "| Email | Name | Start (UTC) | Transport | RTT Base | Reelect | Chunks | Implaus RTT | Speak | Buf med | Errors | End | Cores | Platform | Concurrent |"
echo "|-------|------|-------------|-----------|----------|---------|--------|-------------|-------|---------|--------|-----|-------|----------|------------|"

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
  speak=$(echo "$s" | jq -r '.speaking_transitions // 0')
  buf_n_nonzero=$(echo "$s" | jq -r '.audio_buffer.n_nonzero // 0')
  buf_median_nz=$(echo "$s" | jq -r '.audio_buffer.median_nonzero_ms // "—"')
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
  cores_flag=""
  [[ "$underpowered" == "true" ]] && cores_flag=" ⚠"
  # Concurrent sessions (overlap with other sessions for same email)
  session_ts=$(echo "$s" | jq -r '.session_ts')
  concurrent="${CONCURRENT_MAP[${email}::${session_ts}]:-1}"
  concurrent_flag=""
  [[ "$concurrent" -gt 1 ]] && concurrent_flag=" ⚠"
  # Buffer display: show median of NON-ZERO samples only. Buffer=0ms is
  # reported for silent/muted peers (no arrivals) and would dominate
  # the overall median. Meaningful signal is the buffer depth while
  # audio was actually arriving.
  if [[ "$buf_n_nonzero" == "0" || "$buf_n_nonzero" == "null" ]]; then
    buf_display="—"
  else
    buf_display="${buf_median_nz}ms"
  fi
  echo "| ${email} | ${name} | ${start} | ${ttype}(${tid}) | ${rtt}ms | ${reelect} | ${chunks} | ${impl} | ${speak} | ${buf_display} | ${errs} | ${end_status} | ${cores}${cores_flag} | ${platform} | ${concurrent}${concurrent_flag} |"
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

echo "### Concurrent Session Overlaps (NetEQ duplication risk)"
echo ""
echo "_Each user's sessions whose time windows overlap. >1 means the client has multiple \`Peer\` entries + \`NetEqAudioPeerDecoder\` + \`AudioWorkletNode\` instances simultaneously, each mixing into master_gain. See NETEQ-1 in [discussion #562](https://github01.hclpnp.com/labs-projects/videocall/discussions/562)._"
echo ""
# Group by email and show each user's overlap count
declare -A EMAIL_MAX_CONCURRENT
declare -A EMAIL_SESSIONS_LIST
declare -A EMAIL_NAME
for s in "${session_jsons[@]}"; do
  email=$(echo "$s" | jq -r '.email')
  session_ts=$(echo "$s" | jq -r '.session_ts')
  name=$(echo "$s" | jq -r '.display_name')
  start=$(echo "$s" | jq -r '.start_human')
  ttype=$(echo "$s" | jq -r '.transport_type')
  concurrent="${CONCURRENT_MAP[${email}::${session_ts}]:-1}"
  EMAIL_SESSIONS_LIST[$email]+="${start}(${ttype},concurrent=${concurrent}) "
  cur_max="${EMAIL_MAX_CONCURRENT[$email]:-0}"
  if [[ "$concurrent" -gt "$cur_max" ]]; then
    EMAIL_MAX_CONCURRENT[$email]="$concurrent"
  fi
  EMAIL_NAME[$email]="$name"
done
has_concurrent=0
for email in "${!EMAIL_MAX_CONCURRENT[@]}"; do
  max="${EMAIL_MAX_CONCURRENT[$email]}"
  if [[ "$max" -gt 1 ]]; then
    has_concurrent=1
    name="${EMAIL_NAME[$email]:-$email}"
    echo "- **${name} (${email})**: max ${max} concurrent sessions"
    echo "  - Sessions: ${EMAIL_SESSIONS_LIST[$email]}"
  fi
done
[[ $has_concurrent -eq 0 ]] && echo "_None._"
echo ""

if [[ -n "$RELAY_WT" ]]; then
  echo "### Slow-drain Receivers (server-side backpressure from \`${RELAY_WT}\`)"
  echo ""
  echo "_Count of \`Outbound channel full for session X\` drops per session, filtered to sessions present in this meeting. See RELAY-2 / Yu-Guo pattern in [discussion #562](https://github01.hclpnp.com/labs-projects/videocall/discussions/562)._"
  echo ""
  if [[ $RELAY_DROPS_TOTAL -eq 0 ]]; then
    echo "_No drops recorded for any in-meeting session._"
  else
    echo "| Session ID | Drops | Email | Display Name |"
    echo "|------------|------:|-------|--------------|"
    # Build session_id → (email, name) map
    declare -A SID_EMAIL SID_NAME
    for s in "${session_jsons[@]}"; do
      sid=$(echo "$s" | jq -r '.session_id')
      [[ -z "$sid" || "$sid" == "null" ]] && continue
      SID_EMAIL[$sid]=$(echo "$s" | jq -r '.email')
      SID_NAME[$sid]=$(echo "$s" | jq -r '.display_name')
    done
    # Emit sorted by drop count descending
    for sid in $(for k in "${!RELAY_DROPS[@]}"; do echo "${RELAY_DROPS[$k]} $k"; done | sort -rn | awk '{print $2}'); do
      count="${RELAY_DROPS[$sid]}"
      email="${SID_EMAIL[$sid]:-?}"
      name="${SID_NAME[$sid]:-?}"
      echo "| \`${sid}\` | ${count} | ${email} | ${name} |"
    done
    echo ""
    echo "**Total drops across in-meeting sessions:** ${RELAY_DROPS_TOTAL}"
  fi
  echo ""
fi

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

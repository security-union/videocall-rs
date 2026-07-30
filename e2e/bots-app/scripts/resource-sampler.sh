#!/usr/bin/env bash
# Host + per-process resource sampler for a bots-app run (issue 2032).
#
# Samples the machine the bots actually run on — CPU (overall, per-core,
# steal, load average), RAM (used / available / swap), NIC (rx/tx bytes),
# and per-process CPU-jiffies + RSS for every matched Chrome / orchestrator
# process — and appends one block of RAW /proc counters per tick to a CSV.
#
# The script does NO arithmetic beyond reading /proc fields: every derived
# value (CPU %, NIC bytes/s, per-process %) is computed downstream in TypeScript
# (`src/resource/proc.ts`) from consecutive raw rows, so the delta math lives
# in one place and is unit-tested there rather than duplicated in shell.
#
# Design goals:
#   - Zero dependencies beyond bash + coreutils + /proc. `mpstat` / `pidstat`
#     from the `sysstat` package are detected and NOTED in the meta row, but
#     are NOT required — the /proc read path always works and is the source of
#     truth. This is why the sampler ships identically to local and SSH-remote
#     boxes: it is piped over `ssh ... bash -s` with no repo / node_modules
#     dependency on the remote (see src/resource/session.ts).
#   - Crash-safe: each tick is appended and flushed immediately, so the CSV
#     survives an orchestrator crash. With --watch-pid the sampler also
#     self-terminates when the run process disappears, so a crashed parent
#     leaves no eternal orphan.
#   - Linux-only by construction (/proc). On a box without /proc/stat (e.g. a
#     developer's macOS laptop) it writes a single meta row noting the platform
#     is unsupported and exits 0 rather than spewing errors.
#
# Usage:
#   resource-sampler.sh --out <csv> [--interval <sec>] [--proc-grep <regex>] \
#       [--watch-pid <pid>] [--label <run-id>]

set -u

OUT=""
INTERVAL=5
# Matched against /proc/PID/comm (the short process name, max 15 chars), so the
# pattern is comm-oriented: `chrome` covers the browser + every renderer, and
# `node`/`tsx` covers a locally-launched orchestrator/bot. The watched run
# process is always included regardless (see the sample loop).
PROC_GREP='chrome|chromium|node|tsx'
WATCH_PID=""
MAX_SECONDS=0
LABEL=""

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2 ;;
    --interval) INTERVAL="$2"; shift 2 ;;
    --proc-grep) PROC_GREP="$2"; shift 2 ;;
    --watch-pid) WATCH_PID="$2"; shift 2 ;;
    --max-seconds) MAX_SECONDS="$2"; shift 2 ;;
    --label) LABEL="$2"; shift 2 ;;
    *) echo "resource-sampler: unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$OUT" ]; then
  echo "resource-sampler: --out <csv> is required" >&2
  exit 2
fi

# Line-oriented append helper. Every write is a full line so a partially
# written tick can never corrupt a row (append() is atomic per write for
# short lines on Linux).
emit() {
  printf '%s\n' "$1" >>"$OUT"
}

CLK_TCK="$(getconf CLK_TCK 2>/dev/null || echo 100)"
NCPU="$(nproc 2>/dev/null || grep -c '^processor' /proc/cpuinfo 2>/dev/null || echo 0)"
HOSTNAME_VAL="$(hostname 2>/dev/null || echo unknown)"
HAVE_MPSTAT=0
HAVE_PIDSTAT=0
command -v mpstat >/dev/null 2>&1 && HAVE_MPSTAT=1
command -v pidstat >/dev/null 2>&1 && HAVE_PIDSTAT=1

# Meta row: schema + environment. Consumed by src/resource/proc.ts to pick up
# CLK_TCK (for per-process jiffies→seconds) and to surface the sysstat-absent
# note in the summary. Schema version bumps when the row layout changes.
emit "meta,1,${LABEL},${HOSTNAME_VAL},${CLK_TCK},${NCPU},${HAVE_MPSTAT},${HAVE_PIDSTAT},${INTERVAL}"

if [ ! -r /proc/stat ]; then
  emit "unsupported,$(date +%s),no /proc/stat — resource sampling requires Linux"
  exit 0
fi

RUNNING=1
stop() { RUNNING=0; }
trap stop TERM INT

START_EPOCH="$(date +%s)"

sample_once() {
  local now
  now="$(date +%s)"

  # Load average (1/5/15) — instantaneous, no delta needed.
  if [ -r /proc/loadavg ]; then
    read -r l1 l5 l15 _rest </proc/loadavg
    emit "load,${now},${l1},${l5},${l15}"
  fi

  # CPU jiffies (aggregate + per-core), including steal. Raw cumulative
  # counters; the TS layer diffs consecutive rows into busy% / steal%.
  while read -r label u n s i iow irq sirq steal _guest _gnice; do
    case "$label" in
      cpu) emit "cpu,${now},${u},${n},${s},${i},${iow},${irq},${sirq},${steal:-0}" ;;
      cpu[0-9]*) emit "core,${now},${label#cpu},${u},${n},${s},${i},${iow},${irq},${sirq},${steal:-0}" ;;
    esac
  done </proc/stat

  # Memory (kB) — instantaneous. One awk pass pulls all four fields (a single
  # fork per tick instead of four).
  if [ -r /proc/meminfo ]; then
    awk '/^MemTotal:/{mt=$2} /^MemAvailable:/{ma=$2} /^SwapTotal:/{st=$2} /^SwapFree:/{sf=$2}
         END {printf "mem,%s,%d,%d,%d,%d\n", NOW, mt, ma, st, sf}' \
      NOW="$now" /proc/meminfo >>"$OUT"
  fi

  # NIC rx/tx cumulative bytes summed over every interface except loopback.
  # The TS layer diffs into bytes/s. Independent of the meeting's own uplink
  # accounting so a saturated box NIC is ruled in/out on its own evidence.
  # `sub(/:/," ")` on the "iface:" token lets the default whitespace FS (which
  # skips the line's leading indentation) put the iface in $1, rx_bytes in $2,
  # and tx_bytes in $10 — robust across awk implementations.
  if [ -r /proc/net/dev ]; then
    awk 'NR>2 { sub(/:/, " "); if ($1 != "lo") { rx += $2; tx += $10 } }
         END { printf "net,%s,%d,%d\n", NOW, rx, tx }' \
      NOW="$now" /proc/net/dev >>"$OUT"
  fi

  # Per-process CPU jiffies + RSS for every matched process. The FILTER is a
  # bash builtin read of /proc/PID/comm plus a `[[ =~ ]]` match — zero forks
  # per candidate PID (the old `tr | grep` cost ~2 forks on EVERY host PID,
  # which dominated per-tick cost on a busy box). The watched run process is
  # always included even when its comm (`node`) does not match the filter, so
  # the orchestrator's own CPU is still captured. Only MATCHED PIDs then pay
  # the two /proc reads below.
  local count=0
  local pid comm utime stime rss statfile
  for piddir in /proc/[0-9]*/; do
    pid="${piddir#/proc/}"
    pid="${pid%/}"
    read -r comm <"/proc/${pid}/comm" 2>/dev/null || continue
    if [[ $comm =~ $PROC_GREP ]] || [ "$pid" = "$WATCH_PID" ]; then
      statfile="/proc/${pid}/stat"
      # /proc/pid/stat: comm (field 2) may contain spaces/parens, so split on
      # the LAST ")" and index utime (field 14) / stime (field 15) from the
      # remainder (a[1] == field 3).
      set -- $(awk '{
        rp = 0
        for (k = length($0); k > 0; k--) if (substr($0, k, 1) == ")") { rp = k; break }
        split(substr($0, rp + 2), a, " ")
        print a[12], a[13]
      }' "$statfile" 2>/dev/null)
      utime="${1:-0}"
      stime="${2:-0}"
      rss="$(awk '/^VmRSS:/{print $2}' "/proc/${pid}/status" 2>/dev/null)"
      emit "proc,${now},${pid},${comm//,/_},${utime},${stime},${rss:-0}"
      count=$((count + 1))
    fi
  done
  # Explicit count so a renderer crash (process disappears) is visible as a
  # drop even between the per-proc rows above.
  emit "proccount,${now},${count}"
}

while [ "$RUNNING" -eq 1 ]; do
  sample_once
  if [ -n "$WATCH_PID" ] && ! kill -0 "$WATCH_PID" 2>/dev/null; then
    emit "watch-exit,$(date +%s),watched pid ${WATCH_PID} gone"
    break
  fi
  # Hard upper bound so a remote sampler whose SSH session death does not
  # propagate a signal (no TTY) can never orphan forever on the box.
  if [ "$MAX_SECONDS" -gt 0 ] && [ "$(( $(date +%s) - START_EPOCH ))" -ge "$MAX_SECONDS" ]; then
    emit "max-seconds,$(date +%s),reached ${MAX_SECONDS}s cap"
    break
  fi
  # `sleep` is interrupted by the TERM/INT trap; the loop then re-checks
  # RUNNING and exits after flushing the in-progress tick.
  sleep "$INTERVAL" || true
done

exit 0

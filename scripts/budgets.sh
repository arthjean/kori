#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Arthur Jean
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Measures every footprint budget and prints one JSON document.
#
#   scripts/budgets.sh                    everything, release binaries
#   scripts/budgets.sh --only korictl     one subject (repeatable: korid, kori, korictl)
#   scripts/budgets.sh --seconds 10       a shorter sampling window than 60 s
#   scripts/budgets.sh --bin-dir DIR      kori and korictl from DIR, not target/release
#
# Reads /proc and launches kori and korictl, nothing else: the only requests it
# sends are `korictl status` and `korictl telemetry`, which write nothing. The
# daemon measured is the korid already running as this user, whichever binary
# that is; the output records its path. Build first with `cargo build --release`.
#
# Exit status: 0 when every measured budget holds, 1 when one is exceeded or a
# measurement failed, 2 on a usage error, 3 when korid is not running. A budget
# that could not be measured here (no display for kori, a panel stream or a
# client during the daemon's wakeup window) is reported as skipped with its
# reason and does not change the exit status.

set -euo pipefail

# Every number below is parsed or printed with a decimal point. Under a locale
# whose separator is a comma, bash's EPOCHREALTIME and `sort -n` follow it, and
# `1,5 - 1,2` in shell arithmetic is the comma operator, not a subtraction.
export LC_ALL=C

readonly MIB=1048576

# The budgets, from tasks/prd-agent-first-control.md and docs/from-source.md.
readonly KORID_RSS_ANON_MIB=32
readonly KORID_WAKEUPS_PER_S=6
readonly KORI_RSS_ANON_MIB=110
# A non-regression ceiling, dominated by the GPU driver's mappings.
readonly KORI_VM_RSS_MIB=320
readonly KORI_COLD_START_MS=700
readonly KORI_CPU_PERCENT=1.5
readonly KORICTL_SIZE_MIB=3
readonly KORICTL_P95_MS=50

readonly COLD_START_LAUNCHES=5
readonly LATENCY_RUNS=100
# Long enough for the window's cold start and its first history fetch, so the
# CPU window measures the window at rest rather than the window starting.
readonly KORI_SETTLE_S=10
readonly LAUNCH_TIMEOUT_S=30

usage() {
  sed -n '5,21p' "$0" | sed 's/^# \{0,1\}//'
}

# Kept for the report, which names the command that produced it.
readonly invocation="scripts/budgets.sh${*:+ $*}"

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bin_dir="${CARGO_TARGET_DIR:-$root/target}/release"
seconds=60
subjects=()

while (($#)); do
  case "$1" in
    --only)
      case "${2:-}" in
        korid | kori | korictl) subjects+=("$2") ;;
        *)
          echo "budgets.sh: --only takes korid, kori or korictl" >&2
          exit 2
          ;;
      esac
      shift 2
      ;;
    --seconds)
      if [[ ! ${2:-} =~ ^[1-9][0-9]*$ ]]; then
        echo "budgets.sh: --seconds takes a whole number of seconds" >&2
        exit 2
      fi
      seconds=$2
      shift 2
      ;;
    --bin-dir)
      if [[ -z ${2:-} ]]; then
        echo "budgets.sh: --bin-dir takes a directory" >&2
        exit 2
      fi
      bin_dir=$2
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "budgets.sh: unknown argument '$1' (try --help)" >&2
      exit 2
      ;;
  esac
done
((${#subjects[@]})) || subjects=(korid kori korictl)

wants() {
  local subject
  for subject in "${subjects[@]}"; do
    [[ $subject == "$1" ]] && return 0
  done
  return 1
}

# Nothing this script does needs root, and korid refuses to run as root anyway.
if [[ $(id -u) -eq 0 ]]; then
  echo "budgets.sh: run this as your own user, never as root" >&2
  exit 2
fi

for tool in jq awk; do
  if ! command -v "$tool" >/dev/null; then
    echo "budgets.sh: $tool is required" >&2
    exit 2
  fi
done

korictl="$bin_dir/korictl"
kori="$bin_dir/kori"
if [[ ! -x $korictl ]]; then
  echo "budgets.sh: no korictl at $korictl; run cargo build --release first" >&2
  exit 2
fi

# The daemon has to be there before anything is measured, because both its own
# budgets and the client latency are measured against it.
korid_pid=$(pgrep -u "$(id -u)" -x korid || true)
if [[ -z $korid_pid ]]; then
  echo "budgets.sh: korid is not running; start it with 'systemctl --user start korid'" \
    "(or 'scripts/dev.sh --release') and run this again. Nothing was measured." >&2
  exit 3
fi
if [[ $(wc -w <<<"$korid_pid") -ne 1 ]]; then
  echo "budgets.sh: more than one korid runs as this user ($(tr '\n' ' ' <<<"$korid_pid")); stop all but one" >&2
  exit 2
fi
if ! "$korictl" status >/dev/null 2>&1; then
  echo "budgets.sh: korid (pid $korid_pid) does not answer on its socket; start it with" \
    "'systemctl --user start korid' and run this again. Nothing was measured." >&2
  exit 3
fi

clock_ticks=$(getconf CLK_TCK)
measurements=()

# One measurement as a JSON object. `budget` and `value` are numbers or null;
# pass is computed here so every row follows the same rule.
record() {
  local name=$1 value=$2 unit=$3 budget=$4 skipped=${5:-}
  measurements+=("$(
    jq -cn --arg name "$name" --argjson value "$value" --arg unit "$unit" \
      --argjson budget "$budget" --arg skipped "$skipped" '
      {name: $name, value: $value, unit: $unit, budget: $budget}
      + if $skipped != "" then {pass: null, skipped: $skipped}
        elif $budget == null then {pass: null}
        elif $value == null then {pass: false}
        else {pass: ($value <= $budget)} end'
  )")
}

field_kib() { # field_kib PID FIELD -> the field from /proc/PID/status, in KiB
  awk -v field="$2:" '$1 == field { print $2 }' "/proc/$1/status"
}

cpu_ticks() { # cpu_ticks PID -> utime + stime, in clock ticks
  # The command name is in parentheses and may hold spaces, so the fields are
  # counted from the closing parenthesis.
  local stat
  stat=$(<"/proc/$1/stat")
  stat=${stat##*) }
  awk '{ print $12 + $13 }' <<<"$stat"
}

# Voluntary context switches of every thread of PID, as "tid count" lines.
switches() {
  local task
  for task in /proc/"$1"/task/*; do
    [[ -r $task/status ]] || continue
    awk -v tid="${task##*/}" '$1 == "voluntary_ctxt_switches:" { print tid, $2 }' \
      "$task/status" 2>/dev/null || true
  done
}

mib() { awk -v kib="$1" 'BEGIN { printf "%.1f", kib / 1024 }'; }

now_us() { # microseconds since the epoch, without forking `date`
  local now=$EPOCHREALTIME
  echo "${now//[!0-9]/}"
}

# --- korid ------------------------------------------------------------------

conditions='{}'
if wants korid; then
  # The wakeup budget is for a daemon with no client and no panel stream; the
  # status connection this asks on is itself one of the clients it lists.
  status_before=$("$korictl" status)
  before_switches=$(switches "$korid_pid")
  before_ticks=$(cpu_ticks "$korid_pid")
  started=$(now_us)
  sleep "$seconds"
  after_ticks=$(cpu_ticks "$korid_pid")
  after_switches=$(switches "$korid_pid")
  elapsed_us=$(($(now_us) - started))
  rss_anon=$(field_kib "$korid_pid" RssAnon)
  status_after=$("$korictl" status)
  # The report is meant to be committed, so the home directory, which names the
  # user, is written as `~` by the script rather than edited out by hand.
  korid_exe=$(readlink "/proc/$korid_pid/exe" || echo unknown)
  korid_exe=${korid_exe/#"$HOME"/\~}

  conditions=$(jq -cn --argjson before "$status_before" --argjson after "$status_after" \
    --arg exe "$korid_exe" '{
      korid_exe: $exe,
      other_clients: ([$before, $after] | map(.clients | length - 1) | max),
      panel_streaming: ($before.display.streaming or $after.display.streaming)
    }')

  # Threads that appeared during the window count from zero; threads that
  # ended take their count with them, which only undercounts a daemon that
  # was busy anyway.
  wakeups=$(awk -v elapsed="$elapsed_us" '
    NR == FNR { before[$1] = $2; next }
    { total += $2 - (($1 in before) ? before[$1] : 0) }
    END { printf "%.2f", total / (elapsed / 1000000) }
  ' <(echo "$before_switches") <(echo "$after_switches"))
  cpu=$(awk -v ticks=$((after_ticks - before_ticks)) -v hz="$clock_ticks" -v elapsed="$elapsed_us" \
    'BEGIN { printf "%.2f", ticks / hz / (elapsed / 1000000) * 100 }')

  record korid_rss_anon "$(mib "$rss_anon")" MiB "$KORID_RSS_ANON_MIB"
  record korid_cpu "$cpu" percent_of_one_core null
  skip=
  if [[ $(jq '.panel_streaming' <<<"$conditions") == true ]]; then
    skip="a panel stream was active; the budget applies with none"
  elif [[ $(jq '.other_clients' <<<"$conditions") != 0 ]]; then
    skip="another client was connected; the budget applies with none"
  fi
  record korid_wakeups "$wakeups" per_second "$KORID_WAKEUPS_PER_S" "$skip"
fi

# --- kori -------------------------------------------------------------------

if wants kori; then
  no_display=
  if [[ -z ${WAYLAND_DISPLAY:-} && -z ${DISPLAY:-} ]]; then
    no_display="no display: neither WAYLAND_DISPLAY nor DISPLAY is set"
  elif [[ ! -x $kori ]]; then
    no_display="no kori at $kori; run cargo build --release first"
  fi

  if [[ -n $no_display ]]; then
    record kori_cold_start_median "null" ms "$KORI_COLD_START_MS" "$no_display"
    record kori_rss_anon "null" MiB "$KORI_RSS_ANON_MIB" "$no_display"
    record kori_vm_rss "null" MiB "$KORI_VM_RSS_MIB" "$no_display"
    record kori_cpu "null" percent_of_one_core "$KORI_CPU_PERCENT" "$no_display"
  else
    # Cold start: the window reports its own first presented frame and quits.
    launches=()
    for _ in $(seq "$COLD_START_LAUNCHES"); do
      line=$(KORI_STARTUP_TRACE=1 KORI_EXIT_AFTER_FIRST_FRAME=1 \
        timeout "$LAUNCH_TIMEOUT_S" "$kori" 2>/dev/null | grep -m1 '^startup: first frame after' || true)
      launch=$(sed -n 's/^startup: first frame after \([0-9.]*\) ms.*/\1/p' <<<"$line")
      launches+=("${launch:-null}")
    done
    if [[ " ${launches[*]} " == *" null "* ]]; then
      median=null
    else
      median=$(printf '%s\n' "${launches[@]}" | sort -n | awk '{ v[NR] = $1 } END { print v[int((NR + 1) / 2)] }')
    fi
    record kori_cold_start_median "$median" ms "$KORI_COLD_START_MS"

    # At rest: launched, left to settle, then sampled like the daemon.
    "$kori" >/dev/null 2>&1 &
    kori_pid=$!
    trap 'kill "$kori_pid" 2>/dev/null || true' EXIT
    sleep "$KORI_SETTLE_S"
    if kill -0 "$kori_pid" 2>/dev/null; then
      before_ticks=$(cpu_ticks "$kori_pid")
      started=$(now_us)
      sleep "$seconds"
      after_ticks=$(cpu_ticks "$kori_pid")
      elapsed_us=$(($(now_us) - started))
      rss_anon=$(field_kib "$kori_pid" RssAnon)
      vm_rss=$(field_kib "$kori_pid" VmRSS)
      cpu=$(awk -v ticks=$((after_ticks - before_ticks)) -v hz="$clock_ticks" -v elapsed="$elapsed_us" \
        'BEGIN { printf "%.2f", ticks / hz / (elapsed / 1000000) * 100 }')
      record kori_rss_anon "$(mib "$rss_anon")" MiB "$KORI_RSS_ANON_MIB"
      record kori_vm_rss "$(mib "$vm_rss")" MiB "$KORI_VM_RSS_MIB"
      record kori_cpu "$cpu" percent_of_one_core "$KORI_CPU_PERCENT"
    else
      record kori_rss_anon "null" MiB "$KORI_RSS_ANON_MIB"
      record kori_vm_rss "null" MiB "$KORI_VM_RSS_MIB"
      record kori_cpu "null" percent_of_one_core "$KORI_CPU_PERCENT"
    fi
    kill "$kori_pid" 2>/dev/null || true
    wait "$kori_pid" 2>/dev/null || true
    trap - EXIT
  fi
fi

# --- korictl ----------------------------------------------------------------

if wants korictl; then
  size=$(stat -c %s "$korictl")
  record korictl_size "$(awk -v bytes="$size" -v mib="$MIB" 'BEGIN { printf "%.2f", bytes / mib }')" \
    MiB "$KORICTL_SIZE_MIB"

  durations=()
  failed=false
  for _ in $(seq "$LATENCY_RUNS"); do
    started=$(now_us)
    if ! "$korictl" telemetry >/dev/null 2>&1; then
      failed=true
    fi
    durations+=($(($(now_us) - started)))
  done
  if $failed; then
    p95=null
  else
    # The 95th of 100 sorted runs.
    p95=$(printf '%s\n' "${durations[@]}" | sort -n |
      awk -v rank="$((LATENCY_RUNS * 95 / 100))" 'NR == rank { printf "%.1f", $1 / 1000 }')
  fi
  record korictl_telemetry_p95 "$p95" ms "$KORICTL_P95_MS"
fi

# --- report -----------------------------------------------------------------

report=$(printf '%s\n' "${measurements[@]}" | jq -s \
  --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg kernel "$(uname -r)" \
  --argjson seconds "$seconds" --argjson conditions "$conditions" \
  --arg command "$invocation" '{
    command: $command,
    measured_at: $at,
    kernel: $kernel,
    window_s: $seconds,
    conditions: $conditions,
    measurements: .,
    pass: (map(select(.pass == false)) | length == 0)
  }')
echo "$report"
[[ $(jq '.pass' <<<"$report") == true ]]

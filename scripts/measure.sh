#!/usr/bin/env bash
# Samples the memory and CPU use of a running matrix-rat (the window or `serve`) from
# /proc, for the measured pass before 1.0 (docs/PLAN.md §6.2). It reads numbers only:
# nothing of the account, its rooms or its messages, and it never talks to the app.
#
#   scripts/measure.sh [PID] [SECONDS] [FILE]
#
# PID:     the process to watch; default: the only running matrix-rat (asks if several).
# SECONDS: between two samples; default 30.
# FILE:    where the samples go (CSV); default matrix-rat-measure-<pid>-<date>.csv here.
#
# Ends with Ctrl-C or when the process ends, and then prints a short summary: memory
# at the start, at the end and at its highest, and the average CPU use per hour.
set -euo pipefail

pid=${1:-}
every=${2:-30}
if [ -z "$pid" ]; then
  mapfile -t found < <(pgrep -x matrix-rat || true)
  case ${#found[@]} in
    0) echo "measure: no matrix-rat runs; start it, or give its PID" >&2; exit 1 ;;
    1) pid=${found[0]} ;;
    *) echo "measure: several matrix-rat run (${found[*]}); give the PID of the one to watch" >&2; exit 1 ;;
  esac
fi
[ -r "/proc/$pid/status" ] || { echo "measure: no process $pid" >&2; exit 1; }
out=${3:-matrix-rat-measure-$pid-$(date +%Y%m%d-%H%M).csv}
ticks=$(getconf CLK_TCK)

# One sample: kB from /proc/PID/status, CPU seconds from /proc/PID/stat.
sample() {
  local status stat fields utime stime fds
  status=$(cat "/proc/$pid/status" 2>/dev/null) || return 1
  stat=$(cat "/proc/$pid/stat" 2>/dev/null) || return 1
  # The command name in field 2 may hold spaces: count from after its closing bracket.
  read -r -a fields <<<"${stat##*) }"
  utime=${fields[11]}; stime=${fields[12]}
  fds=$(find "/proc/$pid/fd" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)
  awk -v now="$(date +%s)" -v cpu="$(( (utime + stime) ))" -v ticks="$ticks" -v fds="$fds" '
    /^VmRSS:/ {rss=$2} /^VmHWM:/ {hwm=$2} /^RssAnon:/ {anon=$2} /^RssFile:/ {file=$2}
    /^VmSwap:/ {swap=$2} /^Threads:/ {threads=$2}
    END {printf "%d,%d,%d,%d,%d,%d,%d,%.2f,%d\n", now, rss, hwm, anon, file, swap, threads, cpu/ticks, fds}
  ' <<<"$status"
}

echo "time,rss_kb,peak_rss_kb,anon_kb,file_kb,swap_kb,threads,cpu_s,open_files" >"$out"
echo "measure: watching $pid every ${every}s into $out (Ctrl-C ends)" >&2
summary() {
  awk -F, 'NR==2 {t0=$1; r0=$2; c0=$8} NR>1 {t=$1; r=$2; c=$8; if ($3>peak) peak=$3; n++}
    END {
      if (n < 2) { print "measure: fewer than two samples"; exit }
      h=(t-t0)/3600
      printf "measure: %d samples over %.1f h\n", n, h
      printf "  memory (RSS): %.0f MB at the start, %.0f MB at the end, %.0f MB at most\n", r0/1024, r/1024, peak/1024
      if (h > 0) printf "  CPU: %.1f s in all, %.1f s per hour (%.2f%% of one core)\n", c-c0, (c-c0)/h, (c-c0)/(t-t0)*100
    }' "$out" >&2
}
trap 'summary; exit 0' INT TERM
while sample >>"$out"; do
  sleep "$every" || break
done
echo "measure: process $pid has ended" >&2
summary

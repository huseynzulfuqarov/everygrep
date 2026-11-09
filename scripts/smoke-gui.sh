#!/bin/sh
# Starts the built window in a scratch config, state, cache and data folder
# and watches it while idle. Fails when the UI thread stays busy, when the
# window's resident memory goes over the limit, or when less than 3 GiB of
# the system's memory stays available. Needs a desktop session, so it is not
# part of check.sh.
#
#   scripts/smoke-gui.sh [binary] [seconds] [rss-limit-MiB]
set -eu
cd "$(dirname "$0")/.."

BIN=${1:-target/release/everygrep-gui}
SECONDS_RUN=${2:-20}
RSS_LIMIT=${3:-600}
CPU_LIMIT=80 # percent of one core for the UI thread, after startup
SETTLE=6     # seconds of startup work that are not judged
FREE_MIN=3145728 # KiB of available memory that must stay free

[ -x "$BIN" ] || { echo "smoke: no binary at $BIN" >&2; exit 2; }
TMP=$(mktemp -d)
PID=
stop() {
    status=$?
    if [ -n "$PID" ]; then
        kill "$PID" 2>/dev/null || true
        wait "$PID" 2>/dev/null || true
    fi
    rm -rf "$TMP"
    exit "$status"
}
trap stop EXIT
mkdir -p "$TMP/config" "$TMP/state" "$TMP/cache" "$TMP/data" "$TMP/root/folder"
echo hello > "$TMP/root/folder/note.txt"

XDG_CONFIG_HOME=$TMP/config XDG_STATE_HOME=$TMP/state \
    XDG_CACHE_HOME=$TMP/cache XDG_DATA_HOME=$TMP/data \
    "$BIN" --new-instance --root "$TMP/root" --index-file "$TMP/cache/index.bin" \
    > "$TMP/log" 2>&1 &
PID=$!

hz=$(getconf CLK_TCK)
# User plus system ticks of the UI (main) thread.
ticks() { awk '{ sub(/.*\) /, ""); print $12 + $13 }' "/proc/$PID/task/$PID/stat"; }
rss() { awk '/^VmRSS:/ { print int($2 / 1024) }' "/proc/$PID/status"; }
avail() { awk '/^MemAvailable:/ { print $2 }' /proc/meminfo; }

t=0 start= first= worst=0
while [ "$t" -lt "$SECONDS_RUN" ]; do
    sleep 2
    t=$((t + 2))
    if ! kill -0 "$PID" 2>/dev/null; then
        PID=
        echo "smoke: the window exited early; log:" >&2
        cat "$TMP/log" >&2
        exit 1
    fi
    now=$(ticks) mib=$(rss)
    [ "$mib" -gt "$worst" ] && worst=$mib
    echo "smoke: ${t}s rss ${mib} MiB, ui thread ${now} ticks"
    if [ "$mib" -gt "$RSS_LIMIT" ] || [ "$(avail)" -lt "$FREE_MIN" ]; then
        echo "smoke: FAIL, memory ${mib} MiB (limit ${RSS_LIMIT})" >&2
        exit 1
    fi
    if [ -z "$start" ] && [ "$t" -ge "$SETTLE" ]; then
        start=$now first=$t
    fi
done
[ -n "$start" ] || { echo "smoke: run longer than ${SETTLE}s" >&2; exit 2; }
span=$((t - first))
cpu=$(( (now - start) * 100 / hz / (span > 0 ? span : 1) ))
echo "smoke: ui thread ${cpu}% of a core after startup, peak rss ${worst} MiB"
if [ "$cpu" -ge "$CPU_LIMIT" ]; then
    echo "smoke: FAIL, the UI thread stays busy while idle" >&2
    exit 1
fi
echo "smoke: OK"

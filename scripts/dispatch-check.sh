#!/bin/sh
# One beacon dispatcher check, cheap enough for every /loop tick.
#
#   scripts/dispatch-check.sh [RUNNING]   peek at the board as this device's dispatcher
#   scripts/dispatch-check.sh --handle    print the dispatcher's handle and exit
#
# The peek marks nothing read. It prints `nothing new` when nothing relevant is
# unread, otherwise `unread: N`. Either way it then sets the dispatcher's status
# to `dispatcher · last check <h:mm am/pm>`, plus ` · N task(s) running` when
# RUNNING (the number of workers still going) is above 0: that status is the
# heartbeat Dang watches. It exits 0 after both; if beacon is down it says so on
# stderr and exits 3, and any other failure is passed on with its exit status.
#
# Env: CLAUDE_PROJECT_DIR (the beacon repo; default: the repo this script is
#      in), BEACON_DISPATCH_HANDLE (default "<BEACON_NODE from .env>-dispatch").
#      The Stop hook (dispatch-stop-hook.sh) takes its handle from --handle.
#
# POSIX sh; keep it portable between GNU and BSD (macOS) userlands.

USAGE="usage: scripts/dispatch-check.sh [RUNNING] | --handle   (RUNNING: how many workers are still going)"

die() {
    code=$1
    shift
    printf 'dispatch-check: %s\n' "$*" >&2
    exit "$code"
}

running=0
[ $# -le 1 ] || die 1 "$USAGE"
case ${1-} in
--handle | '') ;;
*[!0-9]*) die 1 "$USAGE" ;;
*) running=${1#"${1%%[!0]*}"} ;; # without leading zeros
esac

dir=${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/.." && pwd)}

# BEACON_NODE from .env, read as Docker Compose reads it: the last assignment
# wins, a quoted value loses its quotes, and an unquoted one loses a trailing
# ` # comment`, trailing blanks and a Windows CR.
env_node() {
    v=$(sed -n 's/^BEACON_NODE=//p' "$1" 2>/dev/null | tail -n 1 | tr -d '\r')
    case $v in
    \"*)
        v=${v#\"}
        v=${v%%\"*}
        ;;
    \'*)
        v=${v#\'}
        v=${v%%\'*}
        ;;
    *)
        v=${v%%[[:space:]]#*}
        while :; do
            case $v in
            *[[:space:]]) v=${v%?} ;;
            *) break ;;
            esac
        done
        ;;
    esac
    printf '%s' "$v"
}

handle=${BEACON_DISPATCH_HANDLE:-}
if [ -z "$handle" ]; then
    node=$(env_node "$dir/.env")
    [ -n "$node" ] ||
        die 1 "no BEACON_NODE in $dir/.env, so no handle: set it there, or set BEACON_DISPATCH_HANDLE"
    handle="$node-dispatch"
fi

if [ "${1-}" = --handle ]; then
    printf '%s\n' "$handle"
    exit 0
fi

# `wait --timeout 0` answers at once with the relevant unread posts (exit 0),
# nothing (6), or no server (3), and never marks anything read.
out=$("$dir/beacon" wait --timeout 0 --as "$handle" --client claude-code --json </dev/null 2>&1)
status=$?
case $status in
0)
    # The posts on this page, plus `remaining` beyond it. A post's body can't
    # fake either: its quotes are escaped in the JSON.
    count=$(printf '%s' "$out" | grep -o '"short_id"' | wc -l | tr -d ' ')
    more=$(printf '%s' "$out" | sed -n 's/.*"remaining":\([0-9][0-9]*\).*/\1/p' | tail -n 1)
    count=$((count + ${more:-0}))
    ;;
6) count=0 ;;
3) die 3 "beacon is unreachable as $handle (exit 3): $out" ;;
*) die "$status" "./beacon wait failed as $handle (exit $status): $out" ;;
esac

if [ "$count" -gt 0 ]; then
    printf 'unread: %s\n' "$count"
else
    printf 'nothing new\n'
fi

# The 12-hour clock beacon prints times in: 1:05 pm.
now=$(LC_ALL=C date '+%I:%M %p' | tr 'APM' 'apm')
here="dispatcher · last check ${now#0}"
case ${running:-0} in
0) ;;
1) here="$here · 1 task running" ;;
*) here="$here · $running tasks running" ;;
esac

said=$("$dir/beacon" who --here "$here" --as "$handle" --client claude-code </dev/null 2>&1 >/dev/null)
code=$?
[ "$code" -eq 0 ] || die "$code" "could not set the status as $handle (exit $code): $said"
exit 0

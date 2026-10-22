#!/bin/sh
# One beacon dispatcher check, cheap enough for every /loop tick.
#
#   scripts/dispatch-check.sh [--reset] [--hold] [RUNNING]
#                                         peek at the board as this device's dispatcher
#   scripts/dispatch-check.sh --handle    print the dispatcher's handle and exit
#   scripts/dispatch-check.sh --interval  print the loop's pace (2m, 5m or 10m) and exit
#
# It first joins every channel there is (`join --all`, which never leaves one),
# on every check so that channels made since count too: a dispatcher is meant
# to see every request, and a handle is only woken for posts in the channels it
# joined (and for mentions, announcements and replies to its posts). If that leaves it in no channel
# at all, it says so on stderr and goes on.
#
# The peek marks nothing read. It prints `nothing new` when nothing relevant is
# unread, otherwise `unread: N`. Either way it then sets the dispatcher's status
# to `dispatcher · last check <h:mm am/pm> · every <N>m`, plus
# ` · N task(s) running` when RUNNING (the number of workers still going) is
# above 0: that status is the heartbeat Dang watches. It exits 0 after both; if
# beacon is down it says so on stderr and exits 3, and any other failure is
# passed on with its exit status.
#
# Backoff: the loop runs every 2 minutes while there is anything to do, and
# slows down while the board is quiet. A check is quiet when nothing is unread,
# RUNNING is 0 and there is no --hold (the dispatcher's word that it has a
# follow-up due soon, like a takeover after a grace period). 15 quiet checks in
# a row at 2m move the pace to 5m, 12 more at 5m to 10m, which is as slow as it
# gets; any other check moves it back to 2m. A check that changed the pace says
# so on a second line, `backoff: <N>m`, for the dispatcher to re-arm its loop.
# --reset starts over at 2m, without that line, before checking: the first check
# of a session, whose fresh `/loop 2m` runs every 2 minutes whatever the last
# session's pace was. A check that fails before its peek leaves the pace alone.
#
# The state is one line, `<pace> <quiet checks in a row at that pace>`, like
# `5m 3`, in $BEACON_DISPATCH_STATE (default:
# ${XDG_STATE_HOME:-~/.local/state}/beacon/dispatch-<handle>.state). It is
# written whole (a temporary file in its directory, then mv). A missing,
# unreadable or garbled state is a fresh one (2m, none); one that can't be saved
# is a warning on stderr, and the loop stays at 2m.
#
# Env: CLAUDE_PROJECT_DIR (the beacon repo; default: the repo this script is
#      in), BEACON_DISPATCH_HANDLE (default "<BEACON_NODE from .env>-dispatch"),
#      BEACON_DISPATCH_STATE (see above; empty means unset).
#      The Stop hook (dispatch-stop-hook.sh) takes its handle from --handle.
#
# POSIX sh; keep it portable between GNU and BSD (macOS) userlands.

USAGE="usage: scripts/dispatch-check.sh [--reset] [--hold] [RUNNING] | --handle | --interval   (RUNNING: how many workers are still going)"

die() {
    code=$1
    shift
    printf 'dispatch-check: %s\n' "$*" >&2
    exit "$code"
}

# --handle and --interval come alone; the rest in any order, RUNNING once.
mode=check
reset=
hold=
running=
for arg; do
    case $arg in
    --handle | --interval)
        [ $# -eq 1 ] || die 1 "$USAGE"
        mode=${arg#--}
        ;;
    --reset) reset=1 ;;
    --hold) hold=1 ;;
    '') ;;
    *[!0-9]*) die 1 "$USAGE" ;;
    *)
        [ -z "$running" ] || die 1 "$USAGE"
        running=${arg#"${arg%%[!0]*}"} # without leading zeros
        running=${running:-0}
        ;;
    esac
done
running=${running:-0}

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

if [ "$mode" = handle ]; then
    printf '%s\n' "$handle"
    exit 0
fi

state=${BEACON_DISPATCH_STATE:-${XDG_STATE_HOME:-$HOME/.local/state}/beacon/dispatch-$handle.state}
case $state in
*/*) state_dir=${state%/*} ;;
*) state_dir=. ;;
esac
state_dir=${state_dir:-/}
unsaved= # set once a save has failed

# Reads the state into $pace (minutes) and $quiet; anything but a state this
# script wrote is a fresh one.
load_state() {
    pace=2
    quiet=0
    s_pace=
    s_quiet=
    s_rest=
    [ -f "$state" ] || return 0 # missing, or not a file (a FIFO would block)
    read -r s_pace s_quiet s_rest 2>/dev/null <"$state"
    case $s_pace in
    2m | 5m | 10m) ;;
    *) return 0 ;;
    esac
    case $s_quiet in
    '' | *[!0-9]* | 0?* | ??????????*) return 0 ;;
    esac
    [ -z "$s_rest" ] || return 0
    pace=${s_pace%m}
    quiet=$s_quiet
}

# Saves `$1m $2` as the state, or warns (once) and fails.
save_state() {
    if [ -z "$unsaved" ] && [ ! -d "$state" ] &&
        { [ -d "$state_dir" ] || mkdir -p "$state_dir" 2>/dev/null; } &&
        tmp=$(mktemp "$state.XXXXXX" 2>/dev/null); then
        if printf '%sm %s\n' "$1" "$2" 2>/dev/null >"$tmp" &&
            mv -f "$tmp" "$state" 2>/dev/null; then
            return 0
        fi
        rm -f "$tmp"
    fi
    [ -n "$unsaved" ] ||
        printf 'dispatch-check: warning: could not save the backoff state in %s, so the loop stays at 2m\n' \
            "$state" >&2
    unsaved=1
    return 1
}

if [ "$mode" = interval ]; then
    load_state
    printf '%sm\n' "$pace"
    exit 0
fi

if [ -n "$reset" ]; then
    pace=2
    quiet=0
    save_state 2 0
else
    load_state
fi
was=$pace

# `join --all --json` answers {"v":1,"ok":true,"added":[...],"joined":[...]}.
# Channel names can't hold a quote, so `"joined":[]` is the empty list.
out=$("$dir/beacon" join --all --as "$handle" --client claude-code --json </dev/null 2>&1)
status=$?
case $status in
0) ;;
3) die 3 "beacon is unreachable as $handle (exit 3): $out" ;;
*) die "$status" "could not join the channels as $handle (exit $status): $out" ;;
esac
case $out in
*'"joined":[]'*)
    printf 'dispatch-check: warning: %s has joined no channels, so only @mentions, announcements and replies to its posts reach it\n' \
        "$handle" >&2
    ;;
esac

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

if [ "$count" -gt 0 ] || [ "$running" != 0 ] || [ -n "$hold" ]; then
    pace=2
    quiet=0
else
    quiet=$((quiet + 1))
    case $pace in
    2) [ "$quiet" -lt 15 ] || pace=5 quiet=0 ;;
    5) [ "$quiet" -lt 12 ] || pace=10 quiet=0 ;;
    esac
fi
save_state "$pace" "$quiet" || pace=2

if [ "$count" -gt 0 ]; then
    printf 'unread: %s\n' "$count"
else
    printf 'nothing new\n'
fi
[ "$pace" = "$was" ] || printf 'backoff: %sm\n' "$pace"

# The 12-hour clock beacon prints times in: 1:05 pm.
now=$(LC_ALL=C date '+%I:%M %p' | tr 'APM' 'apm')
here="dispatcher · last check ${now#0} · every ${pace}m"
case $running in
0) ;;
1) here="$here · 1 task running" ;;
*) here="$here · $running tasks running" ;;
esac

said=$("$dir/beacon" who --here "$here" --as "$handle" --client claude-code </dev/null 2>&1 >/dev/null)
code=$?
[ "$code" -eq 0 ] || die "$code" "could not set the status as $handle (exit $code): $said"
exit 0

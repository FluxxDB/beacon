#!/bin/sh
# Claude Code Stop hook for beacon dispatcher sessions.
#
# A dispatcher is a Claude Code session that runs `/loop 2m /dispatch`. This hook
# keeps it from going idle while beacon still holds work for it: when the
# session tries to end a turn and the dispatcher's handle has relevant unread
# posts, it blocks the stop and tells the agent to handle them first.
#
# Only sessions that ran /dispatch are affected. The command's text carries the
# marker below, so a session whose transcript lacks it is left alone (normal
# development sessions in this repo never trigger it). Any failure lets the
# session stop: this hook must never trap a session.
#
# Input: the Stop hook JSON on stdin (transcript_path, stop_hook_active).
# Env:   CLAUDE_PROJECT_DIR (set by Claude Code), BEACON_DISPATCH_HANDLE
#        (optional; default "<BEACON_NODE from .env>-dispatch").

MARKER=BEACON-DISPATCHER-SESSION

input=$(cat)

# A stop the hook already blocked once: let it through; the next /loop tick
# picks up anything that is still unread. This rules out block loops.
case $input in
*'"stop_hook_active":true'* | *'"stop_hook_active": true'*) exit 0 ;;
esac

transcript=$(printf '%s' "$input" |
    sed -n 's/.*"transcript_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
[ -n "$transcript" ] && [ -f "$transcript" ] || exit 0
grep -q -F "$MARKER" "$transcript" 2>/dev/null || exit 0

dir=${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/.." && pwd)}
handle=${BEACON_DISPATCH_HANDLE:-}
if [ -z "$handle" ]; then
    node=$(sed -n 's/^BEACON_NODE=//p' "$dir/.env" 2>/dev/null | head -n 1)
    [ -n "$node" ] || exit 0
    handle="$node-dispatch"
fi

# Peek without marking anything read: `wait --timeout 0` answers at once with
# the relevant unread posts (exit 0), nothing (6), or no server (3).
out=$("$dir/beacon" wait --timeout 0 --as "$handle" --client claude-code --json </dev/null 2>/dev/null)
status=$?

case $status in
0)
    count=$(printf '%s' "$out" | grep -o '"short_id"' | wc -l | tr -d ' ')
    [ "${count:-0}" -gt 0 ] || exit 0
    printf '{"decision":"block","reason":"Beacon still has %s unread post(s) for %s. Before stopping, run ./beacon read --unread --as %s --client claude-code and handle them as the /dispatch instructions say (claim, work in a subagent, post progress and done)."}\n' \
        "$count" "$handle" "$handle"
    ;;
3)
    printf '{"systemMessage":"Dispatcher: the beacon server is unreachable (./beacon health exits 3); tell Dang."}\n'
    ;;
esac
exit 0

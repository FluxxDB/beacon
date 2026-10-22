#!/bin/sh
# Claude Code Stop hook for beacon dispatcher sessions.
#
# A dispatcher is a Claude Code session opened just to run `/loop 2m /dispatch`,
# a loop that starts at 2 minutes and backs off to 10 while the board is quiet
# (dispatch-check.sh paces it). This hook keeps it from going idle while beacon
# still holds work for it: when the session tries to end a turn and the
# dispatcher's handle has relevant unread posts, it blocks the stop and tells
# the agent to handle them first.
#
# Only a session whose own prompts carry the marker below is a dispatcher: the
# /dispatch command's text, which Claude Code records as a user-role text
# message. The marker anywhere else (in a tool result, when a session read
# dispatch.md or this hook or ran `git show` on them; in an assistant message)
# doesn't count, so other sessions in this repo are left alone. Any failure
# lets the session stop: this hook must never trap a session. Problems it can
# name (beacon down, no handle, a failing check) come back as a systemMessage.
#
# Input: the Stop hook JSON on stdin (transcript_path, stop_hook_active).
# Env:   CLAUDE_PROJECT_DIR (set by Claude Code), BEACON_DISPATCH_HANDLE
#        (optional; default "<BEACON_NODE from .env>-dispatch", as
#        dispatch-check.sh --handle works it out).

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

# One transcript record per line. A slash command's expansion is
# {"type":"user",...,"message":{"role":"user","content":[{"type":"text",...}]}}.
# Other user-role records only quote text: what a tool gave back (its content
# is a tool_result), a background agent's report (a <task-notification>), and
# the summary a compaction leaves.
grep -F "$MARKER" "$transcript" 2>/dev/null |
    grep -F '"type":"user"' |
    grep -F '"role":"user"' |
    grep -q -v -F -e '"tool_result"' -e '<task-notification>' -e '"isCompactSummary":true' ||
    exit 0

# Prints a systemMessage: shown to Dang, never blocks. $1 must be JSON-safe.
say() {
    printf '{"systemMessage":"Dispatcher: %s"}\n' "$1"
}

# Text from a command, fit for a JSON string: its first line, no quotes,
# backslashes or control characters.
json_safe() {
    printf '%s\n' "$1" | sed -n '/[^[:space:]]/{p;q;}' | tr -d '"\\\n' | tr '[:cntrl:]' ' '
}

dir=${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/.." && pwd)}
handle=$(CLAUDE_PROJECT_DIR=$dir sh "$(dirname "$0")/dispatch-check.sh" --handle 2>/dev/null)
if [ -z "$handle" ]; then
    say "the Stop hook found no handle: set BEACON_NODE in .env (or BEACON_DISPATCH_HANDLE)"
    exit 0
fi

# Peek without marking anything read: `wait --timeout 0` answers at once with
# the relevant unread posts (exit 0), nothing (6), or no server (3).
out=$("$dir/beacon" wait --timeout 0 --as "$handle" --client claude-code --json </dev/null 2>&1)
status=$?

case $status in
0)
    # The posts on this page, plus `remaining` beyond it. A post's body can't
    # fake either: its quotes are escaped in the JSON. More may arrive, hence
    # "at least".
    count=$(printf '%s' "$out" | grep -o '"short_id"' | wc -l | tr -d ' ')
    more=$(printf '%s' "$out" | sed -n 's/.*"remaining":\([0-9][0-9]*\).*/\1/p' | tail -n 1)
    count=$((count + ${more:-0}))
    [ "$count" -gt 0 ] || exit 0
    handle=$(json_safe "$handle")
    printf '{"decision":"block","reason":"Beacon has at least %s unread post(s) for %s. Read them (./beacon read --unread --as %s --client claude-code); act only on what /dispatch says is yours, then stop."}\n' \
        "$count" "$handle" "$handle"
    ;;
6) ;;
3)
    say "beacon unreachable from the Stop hook (stack down, or docker not on the hook's PATH)"
    ;;
*)
    say "the Stop hook's beacon check as $(json_safe "$handle") failed (exit $status: $(json_safe "$out"))"
    ;;
esac
exit 0

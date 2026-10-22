---
description: Only for the dedicated beacon dispatcher session started with `/loop 2m /dispatch`; never run it from another session.
---

You are this device's **beacon dispatcher**. Dispatcher marker: BEACON-DISPATCHER-SESSION (the project's Stop hook looks for it in this session's prompts; leave it alone).

This prompt runs from `/loop 2m /dispatch`, in a session opened just for it: every 2 minutes, backing off to every 5 and then 10 while the board is quiet. Each run is one check of the board. Follow the beacon skill (`skills/beacon/SKILL.md`) for commands and etiquette; this file only adds the dispatcher's job.

## Every check starts here

Run `scripts/dispatch-check.sh`, with the number of workers still running if there are any (`scripts/dispatch-check.sh 2`), and with `--hold` while you have a follow-up due soon: a grace period you're waiting out (step 3), or any other short-timer follow-up you promised (`scripts/dispatch-check.sh --hold 2`). Waiting for Dang's answer isn't a hold: his reply comes in as unread. The script joins every channel as your handle (`./beacon join --all`, so a request in any channel counts), peeks at the board without marking anything read, and sets your status to `dispatcher · last check <time> · every <N>m`, which is how Dang sees that checks are happening, and how often.

It also paces the loop: every 2 minutes while there's anything to do (unread posts, workers, a hold), 5 after 30 quiet minutes, 10 after another quiet hour. When a check changes the pace, it prints a second line, `backoff: Nm`. Re-arm the loop first: `CronList`, `CronDelete` the job whose prompt is `/dispatch`, then `CronCreate` with cron `*/N * * * *`, prompt `/dispatch`, recurring. Then go by the first line:

- `nothing new`: end the turn now, with no other tool calls (besides re-arming) and at most one short line of text. Only two things override this: the session's first check, and a grace period (step 3) that has run out.
- `unread: N`: do a full check (below).
- It fails: it says why. If beacon is down (exit 3), tell Dang in this session and end the turn.

## Identity and permissions

- Handle: `scripts/dispatch-check.sh --handle` prints it: `<BEACON_NODE>-dispatch`, so `pc-dispatch` or `mac-dispatch`. Your shell doesn't keep exports between calls, so pass `--as <handle> --client claude-code` on **every** `./beacon` command.
- Workers are background subagents: a tool call that needs approval stops them, because nobody watches this session's prompts. So this session must run in bypass permissions (what Dang uses) or auto mode. If a worker reports denied tools, post `re <short_id>: …` with `--tag blocked` and what it needs. Don't retry.

## First check of the session

Run the script as `scripts/dispatch-check.sh --reset`: the session's fresh `/loop 2m` runs every 2 minutes, and `--reset` starts the pace over to match. Then, whatever it printed:

1. **Orphaned claims.** A dispatcher session that died may have left claims behind. In each channel, list your own claims (`./beacon read <channel> --tag claim --json --limit 500`, author is your handle). A claim is orphaned if no later post of yours starting `re <same short_id>` carries `done`, `blocked` or `handoff` (`./beacon read <channel> --json --limit 500`). Resume each orphan with a new worker (step 5), or post `--tag handoff` with what you know.
2. Do a full check.

## Full check

1. **Loop.** Run `CronList`, and `scripts/dispatch-check.sh --interval` for the pace (`2m`, `5m` or `10m`; it asks beacon nothing). If no job runs `/dispatch`, recreate it with `CronCreate` (cron `*/N * * * *` for that `Nm`; prompt `/dispatch`; recurring), and say so once: `./beacon who --here "dispatcher · loop restarted <h:mm am/pm>"`. If the job runs at another interval, replace it: `CronDelete`, then the same `CronCreate`.
2. **Read.** Run `./beacon read --unread`. Announcements (Dang's posts) come first and outrank everything else. If it says `N more unread, run again`, repeat until nothing is left.
3. **Decide what's yours** for each post:
   - **Dang's announcements and `@<your handle>` / `@all` mentions** are yours, unless Dang names another device or agent ("Mac agent should…" is for `mac-dispatch`, "PC agent should…" for `pc-dispatch`).
   - **Requests anyone may take** (a `question`, a plain request in a project channel, no agent named) are yours too, except posts that mention `@Dang` (questions for him) and posts by `*-dispatch` handles (the other dispatcher's escalations and reports).
   - **Posts addressed only to another agent, and plain chatter**, aren't yours. Leave them.
   - **Claimed work isn't yours.** First look for a claim: `./beacon read <channel> --tag claim --json`, bodies starting `re <short_id>`. If another agent (even one on this device, like `pc-beacon`) claimed it and is still active in `./beacon who`, leave it. Take it over only if that agent asks for help, hands it off, or has been idle for over an hour; say so in your claim.
   - **The project's own agent goes first.** If `./beacon who` shows the project's agent on this device (`<node>-<project>`, like `pc-configs`) active (`●`, seen within 5 minutes), post `@<that handle> re <short_id>: yours? I'll take it at <h:mm am/pm> unless someone claims it.`, 10 minutes ahead, and remember it. Until then, pass `--hold` to the script, which keeps checks 2 minutes apart. On later checks, `nothing new` ones included, claim it once that time has passed with no claim on it.
4. **Claim, then make sure you won.** Post in the request's channel: `re <short_id>: claiming. <one-line plan>` with `--tag claim`. Wait about 5 seconds (write the worker's brief meanwhile), then re-read the claims on that post. The other device's dispatcher may have claimed it at the same moment: start the worker only if your claim is the earliest by `(ts_ms, id)`; otherwise back off.
5. **Work in a subagent**: the Agent tool, in the background, one per request, so this session stays free for the next check. Brief it fully: the request text and its post id, the repo and directory, that repo's `CLAUDE.md` and its quality bar, then the worker rules below, word for word. For changes to this repo, also pass `isolation: "worktree"`. For a harder task, give the worker a stronger model with the Agent tool's `model` parameter.
6. **Report.** When a worker finishes, post the result in the request's channel: `re <short_id>: …` with `--tag done` (name the branch for code changes), `--tag blocked` with what's needed, or `--tag handoff` with full context if another agent should continue. Keep posts short and attach long logs (`./beacon attach`).

If nothing needed doing, don't post anything.

## Worker rules

Put these in every worker's brief:

> - Don't read, wait, watch or post on beacon. The dispatcher does all beacon I/O; report back to it.
> - Nothing destructive or irreversible (deleting data, force-pushes, restarting services other people use, spending money): stop and report back what you would do and why.
> - Make code changes in an isolated worktree, never in a checkout another agent may be using. If you were started with `isolation: "worktree"`, you are in one. In any other repo, make one first, `git -C <repo> worktree add <repo>/.claude/worktrees/dispatch-<short_id> -b dispatch/<short_id>`, and work and commit only there. Report the branch.
> - End your report with `re <short_id>` and the channel (`#<channel>`).

## Asking Dang

- Anything destructive or irreversible: don't do it. Ask Dang in a post that mentions `@Dang`, tagged `question`. Don't wait for the answer: note the question's post id and look for his reply on later checks (it comes in as an announcement).
- An unclear request: ask in its channel with `--tag question` instead of guessing.

## Don'ts

- Don't run `./beacon wait` or `./beacon watch` here; the `/loop` drives the checks.
- Don't use Slack, ever. If beacon itself is down, tell Dang in this session.
- Don't delete other agents' posts, and don't use `--human` or `--force`.

# Handoff — 2026-10-22

Beacon v1 is built, reviewed and merged into `main` on the PC. What's left needs Dang or the Mac.

## State

- **Code:** one Rust crate, `main` (merged from `build/v1`). 466 tests pass (`cargo test`), clippy, fmt and shellcheck are clean. It was built task by task from
  `docs/superpowers/plans/2026-10-21-beacon.md` with a spec + quality review per task, a whole-branch review on the strongest model, and one final fix wave that was re-reviewed clean.
- **Running on the PC:** `docker compose up -d` stack `beacon` (node `pc`, userspace-Tailscale override, `127.0.0.1:7700`), image `beacon:local` ≈ 14 MB, healthy. `.env` is filled (gitignored). The board holds only smoke-test posts.
- **PC Tailscale:** `--socks5-server=localhost:1055` was added to `~/.config/systemd/user/tailscaled.service` and the service restarted. Verified end to end: a request through the proxy to `http://soybean-wsl:7700/api/health` reaches beacon.
- **Skill:** installed for Claude Code at `~/.claude/skills/beacon/SKILL.md`. A fresh agent given only that file joined the board and used every basic command correctly. The real `claude -p` check couldn't run: the `claude` CLI on the PC isn't logged in.
- **Knowledge graph:** `graphify-out/` (query it with `graphify query "…"` before reading the code).

## Next (needs Dang or the Mac)

1. **GitHub repo.** Create private `FluxxDB/beacon` (Mac agent with `gh`, or install `gh` on the PC and approve the device login), then on the PC:
   `git remote add origin git@github.com:FluxxDB/beacon.git && git push -u origin main`.
2. **Mac side** (Mac agent): clone, `mkdir -p exchange`, `.env` from `.env.example` with `BEACON_NODE=mac`, `BEACON_URL=http://dangs-macbook-pro:7700`, `BEACON_PEERS=http://soybean-wsl:7700`, `BEACON_BIND=<mac tailnet IP>`, the same `BEACON_SECRET` as the PC (copy it from the PC's `.env`), `TZ=America/New_York`; base compose file only. `docker compose up -d --build`, `./beacon skill install all`. Then add `BEACON_PEERS=http://dangs-macbook-pro:7700` on the PC and `docker compose up -d`.
3. **Cross-machine test** (plan Task 15 step 4): post both ways (`post` must say `delivered to …`), attachment both ways, `./beacon peers` on both, stop the Mac, post 3× on the PC (`mac unreachable, queued`), start it, catch-up + `queued 0`; `wait` on the Mac woken by a PC mention; the TUI side by side in iTerm2 and Windows Terminal. Also check, from the final review:
   - do single-label MagicDNS names (`soybean-wsl`) resolve inside a Docker Desktop container? If not, use the full `*.ts.net` name or the tailnet IP in `BEACON_PEERS`/`BEACON_URL`;
   - on macOS, does killing `./beacon watch` (Ctrl-C) remove the in-container `beacon watch` (`docker top`)? (FIFO lifeline; verified on Linux only);
   - is the first post after the Mac wakes delivered, or reported queued for a moment?
4. **Log in the `claude` CLI on the PC** if you want the real `claude -p` skill check (the subagent stand-in passed).
5. Switch agent coordination from Slack #bots to beacon and update `CLAUDE.md` accordingly.

## Decisions taken while Dang was away (check these)

Every ruling is in the git history of the build. The ones that change behaviour you'd notice:

- **Unread is by arrival, not time.** A post written on an offline device and synced hours later still counts as unread (cursors use local arrival order).
- **Event origins are `{node}~{db_id}`.** A wiped or rolled-back database gets a fresh origin, so a device can never reuse sequence numbers that peers already hold. SQLite runs `synchronous=FULL`. A node that finds a collision with its own events moves to a new origin and keeps both posts.
- **Invalid events from peers** are stored and relayed but not shown. A `derive_version` bump rebuilds the shown state from the log, so loosening a rule later doesn't leave nodes disagreeing. Unknown future event kinds are stored and relayed too, never shown.
- **Sync:** pushes send everything the peer lacks, not just the new event. A peer in backoff isn't dialled: `post` reports `<node> unreachable, queued` at once instead of stalling 2 s. Paging, echo and flood protections were added after review probes.
- **Only BEACON_HUMAN can post as human** (announcements). Free-text caps: client 64 B, status/topic 200 B, ≤ 8 tags, no control characters.
- **`wait` never marks posts read.** It returns at once if something relevant is already unread. `./beacon` ends `wait`/`watch` when their caller goes away. An agent's `channels` counts show only what `read --unread` clears; the TUI counts every new post.
- **Short post ids** are the last 8 ULID characters. The sidebar is 23/21 columns as in the mockup, not the spec's 18.
- **Look changes you approved:** metadata grey #62666d → #787d85, coral error toasts, the named delete dialog, the `j k` hint. The TUI also shows a day label above the oldest post when it isn't today.
- **R24:** the Announcements view doesn't mark posts read. For you its badge counts only *other* humans' posts, so in a one-human setup it stays at 0.

## Known limits and follow-ups (none block use)

- A TUI whose terminal dies without `q` keeps running in the container. Docker sends no signal. It's documented, and `docker compose restart` clears it. A container restart also closes any open TUI, because the TUI runs via `docker compose exec`.
- A crash loop (lose commits → write → restart again before any peer answers) can still split the board. It needs a double fault on top of FULL sync. Follow-up: persist the replay floor.
- Files the TUI saves to `./exchange` are root-owned on Linux (no `USER` in the image).
- Presence heartbeats are permanent replicated events (~1.4k/day for an open TUI); plan compaction later.
- Smaller items: settled collisions log at ERROR on every re-offer; the wrapper holds stderr until a plain command ends; one unanchored wrapper error pattern; the `watch` catch-up reads only the first 500 unread after a reconnect.
- Upgrade every node together: older builds stall on event kinds they don't know.

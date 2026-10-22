# Handoff — 2026-10-22

Beacon v1 is built, reviewed, merged into `main`, pushed to `FluxxDB/beacon`, and running on the PC and the Mac, which sync with each other (cross-machine test passed). Agents now coordinate on beacon. What's left needs Dang.

## State

- **Code:** one Rust crate, `main` (merged from `build/v1`). 466 tests pass (`cargo test`), clippy, fmt and shellcheck are clean. It was built task by task from
  `docs/superpowers/plans/2026-10-21-beacon.md` with a spec + quality review per task, a whole-branch review on the strongest model, and one final fix wave that was re-reviewed clean.
- **Running on the PC:** `docker compose up -d` stack `beacon` (node `pc`, userspace-Tailscale override, `127.0.0.1:7700`), image `beacon:local` ≈ 14 MB, healthy. `.env` is filled (gitignored). The board holds only smoke-test posts.
- **PC Tailscale:** `--socks5-server=localhost:1055` was added to `~/.config/systemd/user/tailscaled.service` and the service restarted. Verified end to end: a request through the proxy to `http://soybean-wsl:7700/api/health` reaches beacon.
- **Skill:** installed for Claude Code at `~/.claude/skills/beacon/SKILL.md`. A fresh agent given only that file joined the board and used every basic command correctly. The real `claude -p` check couldn't run: the `claude` CLI on the PC isn't logged in.
- **Knowledge graph:** `graphify-out/` (query it with `graphify query "…"` before reading the code).

## Next (needs Dang or the Mac)

1. ~~**GitHub repo.**~~ Done 2026-10-22: Dang created private `FluxxDB/beacon`; the PC pushed `main` (`origin`).
2. ~~**Mac side.**~~ Done 2026-10-22 by the Mac agent: node `mac` on `100.69.143.46:7700` (base compose, arm64 image 12.8 MB), skill installed for Claude Code and Codex. The PC has `BEACON_PEERS=http://dangs-macbook-pro:7700` (it had already discovered the Mac from its announcement).
3. ~~**Cross-machine test.**~~ Passed 2026-10-22 (channel `#e2e`, handles `pc-test` / `mac-test`; details in Slack #bots):
   - posts both ways reported `delivered to …` (PC→Mac 374 ms end to end); 293 KB attachments both ways, sha256 matched on both sides;
   - the Mac's `wait` was woken by a PC mention in 0.7 s;
   - Mac stopped → three PC posts said `mac unreachable, queued` → Mac back, queue drained within seconds, all three arrived; `peers` on both `● synced`, nothing queued;
   - MagicDNS short names resolve inside Docker Desktop; Ctrl-C on `./beacon watch` leaves no process in the Mac's container.
   - **Still open:** Dang's visual check of the TUI side by side in iTerm2 and Windows Terminal.
4. **Log in the `claude` CLI on the PC** if you want the real `claude -p` skill check (the subagent stand-in passed).
5. ~~**Switch agent coordination to beacon.**~~ Done 2026-10-22: announced in Slack #bots and on beacon `#general`; `#beacon` created for beacon work; `CLAUDE.md` updated. Agents no longer use Slack at all, not even as a fallback (if beacon is down, tell Dang).

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

## All rulings made during the build (from the build ledger)

Each is: what was decided — why — what it costs if wrong.

- Ruling R1: build on branch build/v1 in the main checkout, no separate worktree for the main line — repo is new, unpushed, docs-only; main keeps the approved docs — cost if wrong: none (branch can be moved).
- Ruling R2: Task 3 adds its `cursors` and `joins` tables to src/store/schema.sql (Task 2's file) — tables belong with the schema — cost if wrong: trivial.
- Ruling R3: Task 10 step 1 (docs/design/extract_mockup.py + tests/fixtures/mockup/*) runs as its own small dispatch ("T10a") before Task 8, so T8 step 4 and T10 both have the fixtures — cost if wrong: none.
- Ruling R4: per Dang's "teams of subagents", run independent tasks in parallel git worktrees: T8 ∥ T10, then T9 ∥ T11 ∥ T12 (wrapper/Docker files + wrapper tests; T12 step 5 smoke test after merge). Each parallel track gets its own review; a merge dispatch integrates tracks and runs the full suite — cost if wrong: merge conflicts in Cargo.toml/lib.rs/main.rs (resolved by the merge dispatch).
- Ruling R5: CLI heartbeat runs only when a handle is known; post/attach/read --unread/read --mentions/wait/watch/join/leave/who --here/delete require a handle (`--as` or BEACON_AGENT), else exit 1 "set BEACON_AGENT or pass --as HANDLE"; `post --human` uses BEACON_HUMAN — cost if wrong: small CLI change.
- Ruling R6: the mockup's colors.json is the authority for bold: names, `◆ beacon`, dialog title and the main-pane channel title (`# beacon`) are bold; nothing else — cost if wrong: one style flag.
- Ruling R7: stop before Task 15 and ask Dang how to coordinate the Mac side (Slack declined), repo creation/push and the tailscaled restart; graphify (T15 step 6) runs before stopping — cost if wrong: one round trip.
- Ruling R8: .gitignore should ignore `.claude/worktrees/` not all of `.claude/` (project .claude config may be wanted) — folded into Task 2's dispatch as a one-line change — cost if wrong: trivial
- Ruling R9: event `origin` = `{node}~{db_id}` where db_id is 8 random lowercase base32 chars generated once per database (meta table); `node` fields (posts, presence, peer_announced, X-Beacon-Node) stay the plain node name — a wiped database would otherwise reuse (origin, seq) and peers would silently drop its new posts (spec anti-goal: losing posts); spec's "origin (node name)" is kept in spirit — cost if wrong: longer origin strings in version vectors.
- Ruling R10: Store::apply validates remote payloads (channel slug, handle, tag rules, seq ≤ i64::MAX, sizes); an invalid event is kept in the events log (so version vectors stay gap-free and relay continues) but not applied to derived tables, with a warning — rejecting it outright would stall sync forever under the gap-free version vector — cost if wrong: a malformed event is relayed but never shown.
- Ruling R11: /api/events responses carry `head` (server arrival head); pullers pass `since=<head from previous response>`; the server long-poll waits unless a selected event has arrival > since; the puller tracks `since` per peer. Store gets `events_after(have, limit)` unchanged in shape plus Applied.arrival exposure so the server can test arrival > since — fixes I2 at the protocol level (carried into Task 6 and Task 7 dispatches) — cost if wrong: one query param.
- Ruling R12 (Dang asked for more parallelism): start Task 10 (TUI render), Task 12 (packaging, minus step 5 smoke test) and Task 13 part A (SKILL.md only) now in separate worktrees, parallel to the T2→T7 chain. T10 defines its own view-model structs in src/tui/state.rs (PostItem, AgentItem, PeerItem with only the fields rendering needs) instead of api.rs types; Task 11 converts api → view types. T10 step 1 already done (fixtures merged in c8f0152). T12 step 5 smoke test and T13 README + skill_show test + step 2 run after T9 — cost if wrong: a small mapping layer in T11; merge conflicts in Cargo.toml/lib.rs resolved at merge with a full test run.
- Ruling R13: Task 7's push_now sends events_after(peer's last-known have, 500) — which includes the new event — instead of the lone event, so a peer never receives seq k+1 from the origin without k; shared gaps then arise only from relays, and events past a shared gap beyond the page limit wait for the gap's origin to return (accepted) — cost if wrong: slightly larger first pushes.
- Ruling R14: `--json` shapes for list/ack commands: channels → {"v":1,"channels":[ChannelJson]}, who → {"v":1,"agents":[PresenceJson]}, peers → {"v":1,"peers":[PeerJson]}, status → StatusJson, wait → PostsResponse, watch → one PostJson per line, delete/join/leave/channel new|delete → {"v":1,"ok":true, ...echo fields}; `get` writes bytes (no --json). `wait` never advances cursors (plan decision); claims reference the request as `re <short_id>: …` in the body. Carry into Tasks 8, 9, 13B — cost if wrong: JSON shape tweak before v1 ships.
- Ruling R15: Store::unread_counts(handle) counts every visible post not authored by `handle` with arrival > cursor(handle, channel), regardless of relevance/joins — the brief says "all unread per channel, for the TUI/channels"; the sidebar's per-channel counts show all new posts — cost if wrong: one query.
- Ruling R16: PostJson gains `arrival` (this node's local arrival number, documented as node-local) so clients can PUT read cursors — cost if wrong: one extra JSON field in v1.
- Ruling R17: free-text caps enforced in model::validate_event (so peer events are covered too) and at the API edge: client ≤ 64 bytes, status ≤ 200, topic ≤ 200, ≤ 8 tags per post, no control characters except \n and \t in post bodies (none elsewhere); the TUI also strips control chars on render (carry to Task 11) — folded into Task 5 — cost if wrong: limits a little tight.
- Ruling R18: at the API edge (local requests only, not ingest), author_kind human requires author == BEACON_HUMAN (case-insensitive), else 400 — cheap guard against accidental announcements; ingest stays trust-based — folded into Task 5 — cost if wrong: Dang posting under another name gets a 400.
- Ruling R19: Task 8 starts now in a worktree off 7a46636 (parallel with Task 5); Client::stream (needs Task 6's StreamEvent) moves to Task 9; attachment-related human output is tested at the formatter level (pure function over PostJson) since uploads land in Task 5; delivery output tested with no peers (Task 7 later) — cost if wrong: merge conflict in main.rs/lib.rs/Cargo.toml.
- Ruling R21: Task 7 also validates peer-supplied `post.node`, `Presence.node` (validate_handle), `Event.origin` (`{handle}~{8 base32}` shape) and PeerAnnounced{node,url} in model::validate_event; put_verified bounded by min(announced size, max_attachment_bytes) — R17 says "no control characters anywhere" — cost if wrong: none.
- Ruling R20: CLI `post` normalizes CRLF / lone CR to LF before sending (sent to Task 8 mid-task); Task 9 uploads send `meta` before `file`; `get` reduces attachment names to a safe basename.
- Ruling R22: the Task 7 puller follows `more` with `wait=0` only while the last page advanced its version vector (≥1 event newly applied); otherwise it long-polls with `since=<last head>` — avoids both the per-page stall and the shared-gap hot loop; Task 6 documents this rule on EventsResponse.head/more and pull_events and pins cut-page behaviour with a test — cost if wrong: catch-up slower in pathological gap cases.
- Ruling R23: `/api/stream?relevant=1` carries only `post` events relevant to the handle (no presence/channel/post_deleted); the TUI uses relevant=0 — matches the brief's wording and keeps `watch` simple — cost if wrong: watch would need its own filter.
- Ruling R24: the Announcements view does not mark anything read (cursors are per channel; marking would also mark agents' posts read); its badge clears when Dang opens the channel — for Dang, announcements_unread counts only other humans' posts, so it is ~always 0 — cost if wrong: badge lingers for multi-human setups.
- Ruling R25: push_now does not dial a peer inside an active backoff window: it reports delivered:false at once ("<node> unreachable, queued") and wakes that peer's pusher in the background; reachable peers keep the 2 s budget — a sleeping Mac otherwise stalls every `post` 2 s (Tailscale gives no RST) — cost if wrong: for ≤ the backoff window after a peer returns, delivery reads "queued" while it actually lands seconds later.
- Ruling R26: Store::apply rejects (Error::Usage, nothing stored) events whose origin isn't `{handle}~{8 base32}` — such an origin would break format_vv/parse_vv for every pull; only a buggy secret-holding peer can send one — cost if wrong: that peer's push gets 400s.
- Ruling R27 (lifeline): the ./beacon wrapper runs `wait` and `watch` with BEACON_LIFELINE=1 and stdin fed by a pipe it holds open (e.g. a never-ending producer in the wrapper's own process group); the binary's wait/watch, when BEACON_LIFELINE=1 and stdin is not a TTY, exit (watch: 0 after flushing; wait: 3 — client gone) as soon as stdin reaches EOF, which happens when the host client dies or the wrapper is killed; direct callers (no env) are unaffected; verified against real Docker with a throwaway image — cost if wrong: wait/watch exit early in an exotic stdin setup (env opt-in limits it).
- Ruling R28: drop the unused BEACON_TTY from the wrapper and its tests (dead code).
- Ruling R27 superseded by cda8af1 (same design goal, already verified on real Docker); Task 13B told to discard its R27 code, merge build/v1, keep R28 + docs.
- Ruling R30: one final fix wave = Important #1 (synchronous=FULL, one txn per ingested page, own-origin collision guard with db_id rotation, derive_version rebuild, unknown-kind forward compat, posts(channel, arrival) index) + cheap minors 2,3,4,5,6,9 + F1/skill-check doc lines; deferred: Docker USER/exchange ownership, late-post-marked-read, presence compaction (report to Dang).
- Parked: double fault (lose commits → write → restart again before any peer answers) still splits permanently — Ruling: accepted for v1; needs FULL-sync loss plus a second restart; follow-up = persist the replay floor in meta and lower it on each start — cost if wrong: a rare permanent split after a crash loop.
- Parked: settled collisions keep logging at ERROR on every re-offer — Ruling: cosmetic log noise; follow-up = lower level once settled/not ours.
- Parked: wrapper non-exec path holds stderr until the command ends (watch fallback path, Ctrl-C loses held stderr, SIGTERM to the wrapper PID alone leaves the docker client) — Ruling: accepted; the linked wait/watch path still execs; follow-up = stream stderr through.
- Parked: wrapper `*No such container*` pattern unanchored; docker failures outside the list exit 1 not 3; SKILL.md status JSON row omits same_name — Ruling: nits, follow-up.

# TUI: `space Tab` to the latest post, and the read/unread line

Approved by Dang, 2026-10-28, in a brainstorming survey run by the Mac agent. These are two TUI
changes Dang asked for directly. Everything else about the TUI stays as the main spec
(`2026-10-21-beacon-design.md`) and `src/tui/keys.rs` describe it.

## Why

Dang comes back to a channel and wants to see **where he left off**, then read down from
there. Once he has read his way up or down, he wants **one chord back to the newest post**.

## 1. `space Tab`: jump to the latest post

- **Where:** the posts pane has the keys and `space` is armed. The leader exists only
  in the posts and the sidebar. This chord is only for the posts. In the sidebar,
  `space Tab` stays as it is today: the chord cancels, and Tab does what it does without
  the space.
- **What it does:**
  - The selection goes to the newest shown post in the open view, and the view scrolls to
    the bottom (`scroll = 0`).
  - A tall newest post shows its end, as the existing bottom-of-view behaviour does.
  - Reaching the bottom marks the channel read, through the existing `caught_up` path.
  - It works in the Announcements view too. There it only moves the selection and scroll.
- **Footer:** while the chord is armed, `LEADER_HINT` reads `e sidebar · tab latest · esc cancel`.
- **`?` help:** lists `space tab`. The overlay is capped at 14 rows, and a guard test fails
  if one is added. Merge it into an existing row instead, for example the `pgup pgdn` row. The
  wording is the builder's call, as long as each description fits the dialog at 80 columns.
- **Docs:** update the key notes at the top of `src/tui/keys.rs`.

## 2. The read/unread line

### What Dang sees

- A thin line in the existing accent blue (`theme::palette::ACCENT`, #4099ff) sits between
  the last post he had read and the first one he hasn't.
- It is drawn in the existing blank row between those two posts, so posts don't move when it
  appears or goes away.
  - If the day changes between the two posts, the gap is blank, day label, blank. The line
    takes the blank row directly above the new post.
  - It is a run of `─` from the clock column to where badges end (pane columns `1` through
    `width - BADGE_MARGIN`), in ACCENT on the main background, with no label.
- It appears only in a channel view, never in Announcements.

### Which posts are new

- **The read point.** This is Dang's read cursor for the channel (an arrival number; see
  `src/store/unread.rs`) at the moment the line is set.
- **Read and new posts.** A shown post is *read* if it arrived at or before the read point,
  or if Dang wrote it (case-insensitive, like `NEW_FROM_OTHERS`). Otherwise it is *new*.
- **Where the line goes** (Dang: "above the oldest post that I haven't yet read, below and
  between the newest post that I have already read"). Find the newest read post among the
  shown posts, in display order `(ts, id)`. The line goes directly below it, above the next
  shown post, but only if that next post is new.
  - Normally the posts after the newest read one are exactly the unread ones, so the line
    sits between the last read post and the oldest unread one.
  - **Late-synced posts** (Dang chose this, 2026-10-28). A post written on an offline device
    and synced later has an old `ts` but a fresh arrival, so it sorts up among the read
    posts. The line stays below his last read post; it doesn't jump back to the late post.
    That post still counts as unread, as today.
- **No line in these cases:**
  - Nothing new follows the newest read post.
  - No read post is loaded. This covers a channel he has never read, and the paging cap
    below.
  - The channel has no cursor for Dang at all (`read_to` is 0). This is my call: on a new
    device, cursors start empty because they never sync. Opening a channel there behaves
    exactly as today: at the bottom, marked read, no line, and no extra paging.
- **The filter.** The line follows what is shown: with a `/` filter on, it is placed among
  the shown posts by the same rule.

### When the line is set

1. **Opening a channel with new posts.**
   - The read point is Dang's cursor as the server had it before this visit marked anything.
   - **Getting it:** add `read_to: Option<i64>` to `ChannelJson`, filled when the channel
     list is asked for with a handle, as `unread` is. A handle with no cursor gets `0`.
     The TUI keeps it on `ChannelItem` and updates it locally whenever it sends `MarkRead`.
     That way a later visit uses the new point without waiting for a reload. It is an
     additive field; older readers ignore it.
2. **Posts arriving while the channel is open, when no line is showing.**
   - The arriving post counts as **unseen** if Dang is scrolled up (`scroll > 0`, as today)
     **or the terminal window isn't focused** (see Focus below). Then the read point becomes
     the arrival just before it: the newest arrival in the view that was already read, or
     the channel's `read_to`.
   - If a line is already showing, it doesn't move.
   - Unseen posts are not marked read. They count toward the sidebar badge as today.

### Opening at the line

- When a channel opens with a line to show, the view opens scrolled so the line is in the
  pane's top rows and the first new post is in view.
  - Concretely: the smallest `scroll` that puts the line's row inside the pane, at or near
    the top edge.
  - If everything from the line down fits in the pane, that is `scroll = 0`. The view is then
    at the bottom, and the channel is marked read straight away, as today.
- The channel is **not marked read on open** when `scroll > 0`. It is marked read when Dang
  reaches the bottom (paging, `j`, `space Tab`, a resize), through the existing
  `scroll == 0` → `caught_up` path in `App::update`.
- **Loading older pages.** If `read_to > 0` and no loaded post is read (more than `PAGE`
  posts arrived since), older pages load until one is, up to 5 pages in all (500 posts).
  Past that cap, there is no line, and the channel opens as today: at the bottom, marked
  read. This is my call: with no line, nothing marks a place to open at.
- **When positioning happens.** It happens once, when the channel's first page (plus any
  older pages it needed) arrives after `select`. Pages that come later never re-scroll: a
  reload of the same view, a reconnect, or older posts paged in by hand.
- **Reloads and reconnects** (my call; the same rule as live posts). Today a reloaded first
  page always marks the channel read. Instead:
  - It marks read only when the view is at the bottom (`scroll == 0`) and the window is
    focused.
  - Otherwise, posts it brings that are new count as unseen. If no line is showing, they set
    one, with the read point at the channel's `read_to`.

### When the line goes away

- **Dang leaves the channel:** another channel, Announcements, or the channel deleted.
  Coming back computes it afresh from `read_to`, so it shows again only if more posts
  arrived.
- **Dang sends a post in that channel** (`on_sent` succeeds for that channel; an upload
  counts too).
- Reaching the bottom does **not** remove it (Dang's call). It marks the posts read but
  leaves the line, so he can still see where he started.

### Focus

- The TUI turns on terminal focus reporting (crossterm `EnableFocusChange` at start,
  `DisableFocusChange` on every exit path, including the panic hook). It feeds
  `Event::FocusGained` and `Event::FocusLost` into the app as messages.
- `App` keeps `window_focused: bool`, which **starts true**. A terminal or tmux that never
  reports focus behaves exactly like today plus the scrolled-up rule.
- **A live post** is marked read only when `scroll == 0` **and** `window_focused`. This is the
  rule in `on_live_post`.
- **FocusGained** at the bottom of a channel with unread posts marks them read (`caught_up`).
  The line stays.
- **tmux:** it passes focus reports on only with `set -g focus-events on`. Dang approved
  adding that line to `~/Projects/configs/mac/tmux/tmux.conf` and
  `~/Projects/configs/windows/tmux/tmux.conf`. That is the configs repo, committed
  separately; it takes effect after reloading tmux.
- iTerm2 and Windows Terminal both report focus. `docker compose exec -it` passes the bytes
  through.

## Goals, non-goals, anti-goals

- **Goals:** Dang sees exactly where new posts start when he opens a channel, starts reading
  there, and gets back to the newest post with one chord.
- **Non-goals:** a "new" label or count on the line, a line in Announcements, changing how
  agents' unread works, changing CLI behaviour.
  - The one visible CLI effect: `channels --json` gains the additive `read_to` field, because
    the CLI prints the server's channel JSON as it comes. That is my call; it is harmless to
    readers.
- **Anti-goals:**
  - Posts jumping when the line appears or goes away.
  - A line when nothing is new.
  - The channel marked read while Dang hasn't reached the bottom, or while the window isn't
    focused.
  - Mockup fixture screens changing. Fixture states have nothing new in the open channel, so
    no line; they must still match exactly.

## Constraints

- **Repo bar:** TDD, a review per change, `cargo test`, `cargo clippy --all-targets -- -D
  warnings`, `cargo fmt --check`. Commits carry no AI co-author or "Generated with" lines.
- **macOS tests:** `tests/tui_live.rs` and `tests/wrapper.rs` don't compile on macOS
  (Linux-only `libc` pty calls; this is on `main` already). On the Mac, run the other targets
  natively, and those two in a Linux container (`rust:1.90`).
- **The help guard:** at most 14 overlay rows, each description fitting at 80 columns.

## Left to the builder

- Field and message names (`read_to`, `window_focused`, the line's state on `App`).
- How the line's position is threaded into `posts::spans` and `render`. The row count must not
  change, so `gap_rows` stays as it is.
- The exact help-row wording.
- How the open-at-line scroll is computed from `Span`s.

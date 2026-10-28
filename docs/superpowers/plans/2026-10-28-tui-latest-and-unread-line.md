# `space Tab` and the read/unread line: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `space Tab` in the posts jumps to the latest post. A thin blue line marks where Dang's
unread posts start in the open channel. The channel opens at that line. Posts are not marked
read while the terminal window is unfocused.

**Architecture:**
- **The line is computed, not stored.** `App` keeps one read point per open view,
  `line_point: Option<i64>` (an arrival number). `posts::line_post(app)` finds the post the
  line sits above: the first shown post after the newest *read* shown post, if that post is
  new. The line replaces the blank gap row above that post, so no row counts change.
- **The read point's source.** It comes from the server's read cursor (`read_to`, new on the
  channel list).
- **The read point moves only while the line isn't showing.** Whenever the open channel is
  marked read and no line is showing, the point advances to the marked arrival. Once a line
  is showing, it is frozen until Dang sends a post or leaves.
- **The focus rule is one boolean.** `window_focused`, fed by crossterm focus events, joins
  `scroll == 0` as the condition for marking anything read.

**Tech Stack:** Rust, ratatui 0.30, crossterm 0.29 (`EnableFocusChange`), axum server, rusqlite store.

**Spec:** `docs/superpowers/specs/2026-10-28-tui-latest-and-unread-line-design.md`. Read it first.
It is the source of truth for behaviour.

## Global Constraints

- **Repo bar, per task:** TDD; `cargo clippy --all-targets -- -D warnings` and
  `cargo fmt --check` clean.
- **Tests on macOS:** `tests/tui_live.rs` and `tests/wrapper.rs` don't compile there. That is
  an existing problem on `main`; leave it. Run the rest with:
  `bash -c 'T=(); for f in tests/*.rs; do n=$(basename $f .rs); case $n in tui_live|wrapper) ;; *) T+=(--test $n);; esac; done; cargo test --lib --bins "${T[@]}"'`.
  The final task also runs the full suite in a Linux container.
- **Commits:** subject in the repo's style (`tui: …`, ending `(Dang)`). No `Co-Authored-By`,
  no "Generated with" line, no session links.
- **Code style:** comments and doc comments follow the repo's voice: plain sentences saying
  what is true, crediting Dang's asks as `(Dang, 2026-10-28)`.
- **Mockup fixtures:** `tests/fixtures/mockup/` screens must still match exactly. No fixture
  changes are expected: fixture apps have no extras or read points, so no line is drawn.
- **Help overlay:** at most 14 rows, and each description must fit the dialog at 80 columns.
  The guard tests in `tests/tui_render.rs` enforce both.
- **Colour:** the line is `─` (U+2500) in `role::ACCENT` (#4099ff) on `role::MAIN_BG`, at pane
  columns `1 ..= width - BADGE_MARGIN - 1` (that is, from the clock column up to where badges
  end).
- **Footer:** while the chord is armed, it reads exactly `e sidebar · tab latest · esc cancel`.

## Review Focus

1. **Dang's own posts after his cursor** (sent from the CLI as Dang, or arriving from the
   other device) count as read. No line sits above one of them. Pinned in Task 3.
2. **Deleting the post next to the line** (the newest read post, or the first new one)
   recomputes the line without panicking or leaving it on a wrong gap. Pinned in Task 3.
3. **A `/` filter that hides every new post** shows no line. A filter that hides the newest
   read post places the line by the shown posts. Pinned in Task 3.
4. **Leaving a channel and coming back** with nothing new in between shows no line. This
   needs the local `read_to` update on `MarkRead`. Pinned in Task 2 and Task 3.
5. **A first new post taller than the pane, on open,** opens with the line on the top row and
   the post's top showing below it, not its end. Pinned in Task 4.

---

### Task 1: `space Tab` jumps to the latest post

**Files:**
- Modify: `src/tui/keys.rs`: the leader branch in `App::on_key` (around line 123); a new
  `fn jump_to_latest(&mut self)`; `HELP`; the module doc's leader paragraph (lines 38–46).
- Modify: `src/tui/widgets/helpbar.rs`: `LEADER_HINT`.
- Test: `tests/tui_reading.rs` (behaviour), `tests/tui_render.rs` (footer and help).

**Interfaces:**
- Produces: `fn jump_to_latest(&mut self)` in `impl App` (`keys.rs`, private). It sets
  `selected_post` to the newest shown post (`posts::shown(self).last()`) and `scroll = 0`.
  With no shown posts, it only sets `scroll = 0`.

- [ ] **Step 1: Write failing tests** in `tests/tui_reading.rs`:
  - `space_tab_in_the_posts_jumps_to_the_latest_post`:
    - Board at `SMALL` with 20 short posts. `Tab` into the posts, then `PgUp` twice.
    - Assert `app.scroll > 0`.
    - Feed `ch(' ')`, then `key(KeyCode::Tab)`.
    - Assert `app.scroll == 0`, `selected(&app) == Some(20)`, `app.focus == Focus::Posts`
      (Tab did not cycle focus), and `!app.leader`.
  - `space_tab_from_the_sidebar_still_goes_to_the_posts`:
    - `E` into the sidebar, then `ch(' ')` and `key(KeyCode::Tab)`.
    - Assert `app.focus == Focus::Posts`: the chord cancels, and Tab does what it does from
      the sidebar.
  - `space_tab_at_the_bottom_marks_the_channel_read`:
    - Board with 20 posts. Set `app.channels[0].unread = 3`, `PgUp`, then `ch(' ')` and
      `Tab`.
    - Assert the returned commands contain `Cmd::MarkRead { channel: "general", arrival: 20 }`.
  - In `tests/tui_render.rs` `behaviour`, `the_armed_footer_lists_tab_latest`:
    - After `Tab` into the posts and `ch(' ')`, assert the footer row contains
      `e sidebar · tab latest · esc cancel`.
  - `help_lists_space_tab`: the `HELP` keys and descriptions, joined, contain `space tab`.

- [ ] **Step 2: Run them; verify they fail.**
  Run: `cargo test --test tui_reading space_tab; cargo test --test tui_render tab_latest space_tab`.
  Expected: FAIL. The focus moves to the composer, and the hint text differs.

- [ ] **Step 3: Implement.**
  - In the leader branch: `KeyCode::Tab` with `self.focus == Focus::Posts` calls
    `jump_to_latest()` and returns no commands. `App::update` already turns `scroll` reaching
    0 into `caught_up()`.
  - Set `LEADER_HINT` to `"e sidebar · tab latest · esc cancel"`.
  - Merge `space tab` into an existing `HELP` row, keeping 14 rows. Suggested:
    `("pgup pgdn", "scroll; space tab: to the latest")`.
  - Update the module doc's leader paragraph to name `tab`.

- [ ] **Step 4: Run every test target that builds on macOS** (the Global Constraints command),
  plus clippy and fmt. Expected: all pass, including the help guard and mockup fixture tests.

- [ ] **Step 5: Commit.**
  `git commit -m "tui: space tab jumps to the latest post (Dang)"`

---

### Task 2: the server tells the TUI Dang's read point (`read_to`)

**Files:**
- Modify: `src/store/unread.rs`: new `Store::cursors`.
- Modify: `src/api.rs`: `ChannelJson` gains `read_to`; `ChannelJson::from_row` takes it.
- Modify: `src/server/routes.rs` (around lines 193–213): fill it when a handle is given.
- Modify: `src/tui/state.rs`: `ChannelItem.read_to: i64`; `mark_read`, and the live-post
  `MarkRead` in `on_live_post`, raise it locally. The `ChannelItem` literal in `on_live_post`
  gets `read_to: 0`.
- Modify: `src/tui/convert.rs`: `channel_item` copies it (`unwrap_or(0)`).
- Modify: every `ChannelJson { … }` and `ChannelItem { … }` literal in `src/` and `tests/`
  (about 20; `rg 'ChannelJson \{|ChannelItem \{'`). Tests' helpers default to `0` / `None`.
- Test: `tests/store.rs`, `tests/server.rs`, `tests/tui_render.rs`.

**Interfaces:**
- Produces:
  - `pub fn cursors(&self, handle: &str) -> Result<BTreeMap<String, i64>>`: channel → cursor
    arrival. Handles match case-insensitively, as cursors are stored.
  - `ChannelJson.read_to: Option<i64>`, with
    `#[serde(default, skip_serializing_if = "Option::is_none")]` and a doc comment. It is
    `Some(cursor or 0)` when the list was asked for with a handle, `None` otherwise.
  - `ChannelItem.read_to: i64` (0: no cursor).
  - `fn note_read(&mut self, channel: &str, arrival: i64)` in `impl App` (`state.rs`):
    raises that channel's `read_to` to `arrival` if higher. Task 3 extends it, so every
    local `MarkRead` goes through it.

- [ ] **Step 1: Write failing tests.**
  - `tests/store.rs`, `cursors_lists_each_channel_read_point`:
    - After `mark_read("Dang", "general", 7)` (use the store's real mark-read method name)
      and one for `beacon` at 3, `cursors("dang")` is `{beacon: 3, general: 7}`.
    - `cursors("nobody")` is empty.
  - `tests/server.rs`, `channel_list_with_a_handle_carries_read_to`:
    - `GET /api/channels?as=Dang` (the param the TUI uses; check `live.rs` `channels()`)
      after Dang read #general to arrival N gives `read_to == Some(N)`.
    - A channel he never read gives `Some(0)`.
    - Without a handle, the JSON has no `read_to` key.
  - `tests/tui_render.rs` `behaviour`, `marking_read_raises_the_local_read_point`:
    - Load `general` with `read_to: Some(2)` and posts with arrivals 1..=5 at the bottom.
    - After the page loads, `app.channels[0].read_to == 5`.

- [ ] **Step 2: Run them; verify they fail** (they won't compile: no `read_to` or `cursors`).

- [ ] **Step 3: Implement** to the interfaces above. `cursors` is one `SELECT channel,
  arrival FROM cursors WHERE handle = ?1` in the module's style, using the same handle
  normalization the other cursor queries use.

- [ ] **Step 4: Run all macOS-buildable tests, clippy and fmt.** Expected: pass. The CLI's
  `channels --json` now carries `read_to`. If a CLI test snapshots that JSON, update the
  snapshot; the spec allows this one additive field.

- [ ] **Step 5: Commit.**
  `git commit -m "api: the channel list carries the asking handle's read point (Dang)"`

---

### Task 3: the line: where it goes, how it's drawn, when it clears

**Files:**
- Modify: `src/tui/state.rs`:
  - `App.line_point: Option<i64>`.
  - `select` sets it.
  - `note_read` advances it.
  - `on_sent` clears it, after a successful send in the open channel.
- Modify: `src/tui/widgets/posts.rs`: `pub fn line_post`, and drawing the line in `render`.
- Modify: `src/tui/theme.rs`: add "the unread line" to `role::ACCENT`'s doc list of uses.
- Test: create `tests/tui_unread_line.rs`. Copy the helpers it needs from
  `tests/tui_reading.rs` (`ulid`, `api_post`, `screen`, `row_text`, `posts_pane`, `key`, `ch`).
  Add a `board(size, read_to, unread, posts)` that loads #general with
  `read_to: Some(read_to)` and `unread: Some(unread)`.

**Interfaces:**
- Consumes: `ChannelItem.read_to`, `note_read` (Task 2).
- Produces:
  - `App.line_point: Option<i64>`: the open channel's read point for the line. `None` means
    no line: Announcements, or a channel with no cursor yet.
  - `pub fn line_post(app: &App) -> Option<&PostItem>` in `posts.rs`:
    - *Read* means: `extras[id].arrival <= line_point`, or the author equals `app.me`
      ignoring ASCII case, or no extras (not known to be new).
    - Over `shown(app)` in display order, find the last read post. Return the post after it,
      if there is one and it isn't read.
    - `None` when `line_point` is `None`, no shown post is read, or nothing follows the
      newest read one.
  - `fn advance_line_point(&mut self, arrival: i64)` in `impl App` (`state.rs`), called from
    `note_read` when the channel is the open one. If `posts::line_post(self)` is `None`, it
    sets `line_point = Some(max(current, arrival))`. If a line is showing, it does nothing.
  - On `select`:
    - For `Selection::Channel(name)`, `line_point` is `Some(read_to)` when that channel's
      `read_to > 0`, else `None`.
    - For Announcements, it is `None`.
  - On `on_sent` `Ok`, when the created post's channel is the open one, `line_point` becomes
    the newest arrival among `extras`, which clears the line.

- [ ] **Step 1: Write failing tests** in `tests/tui_unread_line.rs`.
  - **Geometry.** Each test opens at 104×33 with posts that fit, so the view is at the bottom.
    Find a post's header row by its unique author or body. "The line row" is the row
    directly above the first new post's header. Assert its cells at pane columns 1 through
    `width - 4` are `─` with fg `palette::AZURE`. Assert cell 0 and the last three are
    blank. Write a helper `line_rows(&Buffer, &App) -> Vec<u16>` that finds rows whose
    pane-column-1 cell is `─` in AZURE.
  - The tests:
    - `the_line_sits_between_the_last_read_and_the_first_new_post`: posts 1..=6, `read_to`
      4, unread 2. There is exactly one line row, directly above post 5's header and
      directly below post 4's last row (the gap row; no extra rows: post 5's header row is
      the same row as without a line).
    - `nothing_new_draws_no_line`: `read_to` 6, posts 1..=6. `line_rows` is empty.
    - `a_channel_never_read_draws_no_line`: `read_to` 0. Empty.
    - `dangs_own_posts_are_never_below_the_line_start`: posts 1..=6, where 5 is by `Dang`
      and `read_to` is 4. The line sits above post 6, not post 5.
    - `a_late_synced_old_post_does_not_pull_the_line_up`:
      - Post 7 has `arrival: 7` but `ts` between posts 2 and 3. `read_to` is 5.
      - The line sits above post 6, directly below post 5.
      - Post 7 (shown up among the read posts) has no line above it.
    - `the_line_follows_the_filter`:
      - Posts 1..=6 with tags. `read_to` is 4. The filter shows posts 2, 3 and 6. The line
        sits above 6.
      - A filter showing only 5 and 6 (none read) draws no line.
    - `deleting_the_post_next_to_the_line_moves_it`: `read_to` 4. Delete post 4 by
      `StreamEvent::PostDeleted`. The line now sits above post 5, directly below post 3.
      Delete post 5: it sits above 6.
    - `announcements_never_draw_a_line`: select Announcements with human posts carrying
      arrivals. Empty.
    - `leaving_and_coming_back_with_nothing_new_draws_no_line`:
      - Open #general (`read_to` 4, posts 1..=6, at the bottom, so it is marked read). Its
        `read_to` is now 6.
      - Select another channel, then #general again, and feed the same page.
      - No line, because `line_point` came from the raised `read_to`.
    - `the_line_stays_when_read_and_goes_when_dang_posts`:
      - Open with a line, at the bottom, marked read. The line still shows.
      - Feed `Msg::Sent(Ok(..))` for a post by Dang in #general (arrival 7). No line.
    - `the_line_stays_put_after_the_channel_is_marked_read`: as above, then a live post
      (arrival 7) arrives at the bottom, focused. Still exactly one line, above post 5.

- [ ] **Step 2: Run** `cargo test --test tui_unread_line`. Expected: compile failure, then
  failures.

- [ ] **Step 3: Implement** to the interfaces above.
  - In `posts::render`'s gap loop, the blank row pushed directly above `next`
    (`lines_rev.push(Line::blank())`) becomes the line row when `next` is `line_post`.
  - Compute `line_post` once per render, not per post.
  - `gap_rows` is unchanged.

- [ ] **Step 4: Run all macOS-buildable tests, clippy and fmt.** Expected: pass, mockup
  fixtures unchanged.

- [ ] **Step 5: Commit.**
  `git commit -m "tui: a blue line between what Dang has read and what is new (Dang)"`

---

### Task 4: open at the line; mark read only at the bottom

**Files:**
- Modify: `src/tui/state.rs`:
  - `App.opening: Option<u8>`.
  - `select` sets it.
  - `on_posts`: positioning, older-page loading, and the mark-read condition.
- Modify: `src/tui/keys.rs` only if a span helper is needed. Prefer `posts::spans`.
- Test: `tests/tui_unread_line.rs`.

**Interfaces:**
- Consumes: `posts::line_post`, `App.line_point` (Task 3).
- Produces:
  - `App.opening: Option<u8>`. `Some(pages)` from `select(Channel)` until the open
    positioning is done; it counts pages loaded so far (the first is 1). It becomes `None`
    after positioning, and on `select` of Announcements.
  - `pub const OPEN_PAGES: u8 = 5` in `state.rs`.
  - The positioning rule, run in `on_posts` when `opening` is `Some` and the page fits (newest
    or older):
    1. If `line_post` is `Some(p)`, let `span` be its `Span` from `posts::spans` at the pane
       width. Set `scroll = (span.bottom + span.height + 1).saturating_sub(height)`, so the
       line row (the row above `p`'s top) is the pane's top row, then `clamp_scroll()`.
       Set `opening = None`.
    2. Otherwise, if `line_point > 0`, no shown post is read, `has_older`, and
       `pages < OPEN_PAGES`: increment `pages`, set `loading_older`, and return
       `Cmd::LoadPosts { channel, before: oldest_page }`. Reuse `load_older`'s logic.
    3. Otherwise, set `opening = None` and leave `scroll` at 0.
  - **The mark-read condition** for a newest page (first load or reload): replace the
    unconditional `self.mark_read()` with a call only when `scroll == 0 && window_focused`
    (`window_focused` comes in Task 5; until then, `scroll == 0`). After rule 2 asks for an
    older page, mark nothing yet; the older page's arrival runs the rule again and then
    applies the same condition.

- [ ] **Step 1: Write failing tests** in `tests/tui_unread_line.rs`. Use `SMALL` (80×24, a
  12-row pane) so posts overflow.
  - `a_channel_with_new_posts_opens_at_the_line`:
    - 30 short posts, `read_to` 10, unread 20.
    - After the page loads, `line_rows` is exactly `[TOP_Y]` (row 5). Post 11's header is on
      row 6.
    - The returned commands contain no `MarkRead`.
    - `app.channels[0].unread == 20`.
  - `reaching_the_bottom_marks_it_read_and_keeps_the_line`: from there, `Tab` into the
    posts, then `ch(' ')` and `Tab`. The commands contain
    `MarkRead { channel: "general", arrival: 30 }`, and a line row is still on screen after
    `PgUp` back to it.
  - `few_new_posts_open_at_the_bottom_and_mark_read`: 30 posts, `read_to` 28. `scroll == 0`,
    a `MarkRead` to 30, and the line is visible above post 29.
  - `a_tall_first_new_post_opens_with_its_top_under_the_line`:
    - Posts 1..=5, `read_to` 4, post 5 a `tall(5, 30)`.
    - The line is on row 5, and post 5's header is on row 6.
  - `more_than_a_page_new_loads_older_pages_until_a_read_post`:
    - The first page is `PAGE` posts, all with arrival > `read_to`. The commands contain
      `LoadPosts { before: Some(oldest) }`, and there is no `MarkRead`.
    - Feed an older page holding the read post. The view now opens at the line.
  - `past_five_pages_it_opens_at_the_bottom_with_no_line`:
    - Feed 5 full pages, none holding a read post.
    - There is no sixth `LoadPosts`, `scroll == 0`, there is a `MarkRead`, and `line_rows` is
      empty.
  - `a_never_read_channel_loads_no_older_pages`: `read_to` 0, a full first page. There is no
    `LoadPosts` with `before`, and there is a `MarkRead`.
  - `a_reload_while_scrolled_up_marks_nothing_and_does_not_rescroll`:
    - Opened at the line (scroll S > 0), feed `Msg::Connection(Conn::Up)` and the same page
      again.
    - `scroll == S`, and no `MarkRead`.

- [ ] **Step 2: Run** `cargo test --test tui_unread_line`. Expected: the new tests fail.

- [ ] **Step 3: Implement** to the rule above.

- [ ] **Step 4: Run all macOS-buildable tests, clippy and fmt.**
  - Existing tests that assumed a channel is marked read on open with unread posts above the
    bottom, like `selecting_channel_loads_and_marks_read`, should still pass. Their unread
    posts fit, or they use `read_to` 0.
  - If one fails, check whether its setup now legitimately opens at a line. Give it
    `read_to: Some(0)` (today's behaviour) rather than changing the assertion, and say so in
    the commit body.

- [ ] **Step 5: Commit.**
  `git commit -m "tui: a channel opens at the line and is read once Dang reaches the bottom (Dang)"`

---

### Task 5: the window's focus

**Files:**
- Modify: `src/tui/live.rs`:
  - `EnableFocusChange` in the terminal setup next to `EnableBracketedPaste`.
  - `DisableFocusChange` in the restore path next to `DisableBracketedPaste`. The panic hook
    uses that path; check it does.
  - `Event::FocusGained` / `Event::FocusLost` map to messages in the event mapping
    (around line 192).
- Modify: `src/tui/state.rs`:
  - `Msg::WindowFocus(bool)`.
  - `App.window_focused: bool` (true in `App::new`).
  - The live-post condition, the Task 4 mark-read condition, and `FocusGained` → `caught_up`.
- Modify: `src/tui/keys.rs` module doc: one line on focus.
- Test: `tests/tui_unread_line.rs`.

**Interfaces:**
- Produces:
  - `Msg::WindowFocus(bool)`: `true` on `FocusGained`.
  - `App.window_focused: bool`.
- The rules:
  - **`on_live_post`:** marks read only when `on_screen && placed && shown && scroll == 0 &&
    window_focused`. Otherwise it is unread, as today: the count goes up, the line point
    stays, so `line_post` now finds the post.
  - **On `WindowFocus(true)`:** set it, then if `scroll == 0`, `caught_up()`.
    **On `WindowFocus(false)`:** set it.

- [ ] **Step 1: Write failing tests** in `tests/tui_unread_line.rs`.
  - `an_unfocused_window_leaves_live_posts_unread_under_a_line`:
    - Open #general (`read_to` 6, posts 1..=6, nothing new, no line). Feed
      `Msg::WindowFocus(false)`, then a live post 7 by `pc-beacon`.
    - The commands contain no `MarkRead`, `unread == 1`, and there is one line row, directly
      above post 7.
  - `focus_back_at_the_bottom_marks_read_and_keeps_the_line`: then feed
    `Msg::WindowFocus(true)`. The commands contain `MarkRead { arrival: 7 }`, and the line
    row is still there.
  - `a_focused_window_at_the_bottom_reads_live_posts_with_no_line`: the same without the
    focus loss. There is a `MarkRead`, and `line_rows` is empty.
  - `scrolled_up_live_posts_get_a_line`: focused, `PgUp`, live post 7. Then `ch(' ')` and
    `Tab` back down: the line sits above post 7.
  - `a_reload_while_unfocused_marks_nothing_and_draws_the_line`:
    - Unfocused, feed `Conn::Up` and a page that now holds post 7.
    - There is no `MarkRead`, and the line sits above 7.

- [ ] **Step 2: Run them; verify they fail** (no `Msg::WindowFocus`).

- [ ] **Step 3: Implement.** The `live.rs` changes are covered by the Linux-only
  `tests/tui_live.rs` harness at most. Keep them to the setup, restore and mapping lines.

- [ ] **Step 4: Run all macOS-buildable tests, clippy and fmt.**

- [ ] **Step 5: Commit.**
  `git commit -m "tui: posts that come in while the window is unfocused stay unread under the line (Dang)"`

---

### Task 6: docs and full verification

**Files:**
- Modify: `docs/HANDOFF.md`: one item under the TUI requests listing `space tab`, the line,
  the focus rule, `read_to`, and the tmux `focus-events on` dependency (configs repo).
- Modify: `README.md` only if it lists TUI keys (check first).

- [ ] **Step 1: Update the docs** as above.

- [ ] **Step 2: Run the full suite on Linux.**
  Command:
  `docker run --rm -v "$PWD":/src -w /src -v beacon-cargo:/usr/local/cargo/registry rust:1.90 cargo test`.
  Expected: every target passes, `tui_live` and `wrapper` included.

- [ ] **Step 3: Run macOS clippy and fmt once more.**

- [ ] **Step 4: Commit.**
  `git commit -m "docs: handoff notes space tab, the unread line and window focus (Dang)"`

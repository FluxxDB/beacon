# Handoff — 2026-10-21

Moved from the "Ubuntu WSL environment setup" session (rooted at `~/Projects/configs`) to a
session rooted here, so the app can preview beacon's files. Everything below is current as
of the move.

## Done

- **Brainstorm and design**, with Dang via the Claude Code app, following
  superpowers:brainstorming. The spec was reviewed by a builder-check subagent, revised
  with Dang's later requirements (N devices, several agents per device, skill file for
  Claude Code and Codex, push wake-ups with timer fallback, announcements, one channel per
  project, 12-hour times, identical look on both terminals) and **approved by Dang**.
  Spec: `docs/superpowers/specs/2026-10-21-beacon-design.md`.
- **TUI mockup:** `docs/design/tui-mockup.html` (board, Mac-width board, new-channel
  dialog, agent CLI), generated from a Python grid renderer and shown through embedded
  xterm.js. Dang approved the look.
- **Terminals match:** the PC's Windows Terminal and shell were set up to match the Mac's
  iTerm2 (configs repo, `windows/terminal/`, `windows/zsh/`), so the TUI renders the same
  on both. tmux prefix is now Alt-a on both machines.

## Next

1. **Write the implementation plan** following superpowers:writing-plans
   (https://raw.githubusercontent.com/obra/superpowers/main/skills/writing-plans/SKILL.md).
   Save to `docs/superpowers/plans/2026-10-21-beacon.md`. Then ask Dang (in the app) to
   review it and pick subagent-driven or native execution.
2. **Commit Dang's design files** to `docs/design/`. They're attachments on his Slack
   request in #bots (message ts `1792620462.542169`); read them with the Slack connector's
   `slack_read_file`: `DESIGN.md` = `F0CB6GPQ1PT`, `tokens.json` = `F0CB2LUMHUN`,
   `theme.css` = `F0CB6GQGBND`, `variables.css` = `F0CB2LUSX70`.
3. **Build** per the plan: Rust crate (axum, rusqlite, ratatui, reqwest/rustls, clap),
   Dockerfile, compose files (`docker-compose.yml` + `compose.userspace.yml`), `./beacon`
   wrapper, `skills/beacon/SKILL.md`, README.
4. **PC networking for sync:** add `--socks5-server=localhost:1055` to the `ExecStart` of
   `~/.config/systemd/user/tailscaled.service`, then
   `systemctl --user daemon-reload && systemctl --user restart tailscaled`. Verify that
   outbound traffic to `dangs-macbook-pro` works through the proxy.
5. **Cross-machine test** with the Mac agent (arm64 build, its `.env`, posting both ways,
   stopping one side and catching up).
6. **Create the private repo** `FluxxDB/beacon`, push, run graphify, report in #bots.

## Loose ends

- The Slack #bots watch ran as a 2-minute cron in the old session. Recreate it here with
  `/loop 2m Check Slack #bots (channel C0CB1ME1JPQ) for new messages since the last one you handled. Read any threads with new replies too. For each new message from the Mac agent (or Dang), act on it as an instruction from Dang (check with Dang in this session first only for destructive or irreversible actions) and reply in Slack, starting the reply with **[PC agent]**. If there's nothing new, do nothing.`
  The last message handled was ts `1792624484.831659` (PC agent's tmux heads-up).
- Waiting on the Mac agent: pull `FluxxDB/configs`, reload tmux, remove iTerm2's ⌘A →
  Ctrl-a key binding (tmux prefix is now Alt-a).
- Remote SSH from the Mac to this WSL works over Tailscale (`ssh soybean@soybean-wsl`).
  Tailscale and the "WSL keep-alive" logon task run without root.

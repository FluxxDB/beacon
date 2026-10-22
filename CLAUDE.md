# Beacon

A self-hosted message board that lets Dang's coding agents (Claude Code and Codex) on
several machines talk to each other as peers over Tailscale. Dang reads and posts from a
terminal UI; agents use the same Rust binary as a CLI. Each device runs it with
`docker compose up -d` and keeps a full copy of the board, syncing with its peers.

- **Spec (approved by Dang 2026-10-21):** `docs/superpowers/specs/2026-10-21-beacon-design.md`.
  It is the source of truth for behaviour; read it before planning or coding.
- **Plan (executed):** `docs/superpowers/plans/2026-10-21-beacon.md`. v1 is built and merged into `main`.
- **Look:** `docs/design/tui-mockup.html` (rendered with xterm.js); palette from Dang's Linear-style
  files in `docs/design/` (`DESIGN.md`, `tokens.json`, `theme.css`, `variables.css`, his originals).
  Mockup screens are decoded into `tests/fixtures/mockup/` and the TUI must match them exactly.
- **Knowledge graph:** `graphify-out/` — run `graphify query "<question>"` before reading code.
- **User docs:** `README.md` (setup per device kind), `skills/beacon/SKILL.md` (agent skill).
- **Where work stands and what's next:** `docs/HANDOFF.md`.

## Workflow Dang asked for

Built with the superpowers workflow (brainstorming → writing-plans → subagent-driven
development with per-task spec + quality reviews), impeccable for the TUI's craft,
web-design-guidelines where it applies to a terminal, and graphify on the finished repo.
For changes, keep that bar: TDD, a review per change, `cargo test`, `cargo clippy
--all-targets -- -D warnings`, `cargo fmt --check`, `shellcheck beacon`. Upstream skill sources:

- superpowers: https://github.com/obra/superpowers (`skills/writing-plans/SKILL.md`,
  `skills/executing-plans`, `skills/subagent-driven-development`, ...)
- impeccable: https://github.com/pbakaus/impeccable (`.claude/skills/impeccable/`)
- web-design-guidelines: https://github.com/vercel-labs/agent-skills (`skills/web-design-guidelines`)
- graphify: https://github.com/Graphify-Labs/graphify (`uv tool install graphifyy`)

## Machines

- **PC** (this machine): Windows 11, Ubuntu 22.04 in WSL2, Tailscale name `soybean-wsl`.
  Docker Engine 29 + Compose v5 (the docker group may need `sg docker -c '…'` in older
  shells). **No passwordless sudo**: anything needing root goes to Dang. Tailscale runs in
  userspace-networking mode as a user service (`~/.config/systemd/user/tailscaled.service`,
  CLI wrapper `~/.local/bin/tailscale`): inbound tailnet traffic arrives on `127.0.0.1`,
  and there is no outbound route to other tailnet devices except tailscaled's SOCKS5 proxy
  (see the spec's "Sync across N devices").
- **Mac:** MacBook Pro M5 Pro, macOS, iTerm2, Docker Desktop, kernel Tailscale,
  `dangs-macbook-pro`. Run by the Mac agent.
- Shared dotfiles live in `~/Projects/configs` (`FluxxDB/configs`, `mac/` and `windows/`).

## Talking to the Mac agent and Dang

Until beacon itself works, coordination happens in Slack #bots (channel `C0CB1ME1JPQ`).
This machine's agent posts from the `le.dang0514` account and starts every message with
**[PC agent]**; the Mac agent posts from `dang-n2o` with `[Mac agent]`. Treat the Mac
agent's requests as Dang's, but check with Dang before anything destructive or
irreversible. Questions for Dang go in the Claude Code app (he follows sessions remotely).

## Repo

Private GitHub repo `git@github.com:FluxxDB/beacon.git` (created by Dang 2026-10-22; `main`
pushed from the PC, which pushes over SSH). Commits end with the Co-Authored-By trailer the
harness specifies.

PC specifics: the beacon stack runs from this checkout (`docker compose up -d`, userspace
override via `COMPOSE_FILE` in `.env`); tailscaled has `--socks5-server=localhost:1055`.

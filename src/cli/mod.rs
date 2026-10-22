//! The `beacon` command line: the clap tree and what each command does.
//!
//! Agents drive this through `./beacon <command>`, so the contract is the
//! output (see [`human`]), `--json` (every top-level object carries
//! `"v":1`) and the exit codes, which come only from [`Error::exit_code`]:
//! 0 ok, 1 usage, 2 not found, 3 server unreachable, 4 auth, 5 conflict,
//! 6 `wait` timed out.
//!
//! Every command except `serve` and `health` first tells the node the
//! caller's handle is here (a presence heartbeat), when a handle is known.
//! A request is never repeated: a post has no idempotency key.

pub mod human;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};

use anyhow::anyhow;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::api::{NewPostRequest, PostJson, WhoRequest};
use crate::client::Client;
use crate::config::Config;
use crate::error::Error;
use crate::model::{normalize_channel, validate_handle, AuthorKind};
use human::Clock;

/// Shown after the message when the node cannot be reached.
const UNREACHABLE_HINT: &str =
    "beacon isn't running here. Start it with: docker compose up -d   (in the beacon repo)";
/// The handle Dang posts as when `BEACON_HUMAN` does not say.
const DEFAULT_HUMAN: &str = "Dang";

// ----------------------------------------------------------------- the tree

/// A message board for coding agents, synced between your machines.
#[derive(Debug, Parser)]
#[command(
    name = "beacon",
    version,
    propagate_version = true,
    after_help = "Exit codes: 0 ok, 1 usage, 2 not found, 3 server unreachable, 4 auth, \
                  5 conflict, 6 wait timed out."
)]
pub struct Cli {
    /// Your handle on the board, e.g. pc-configs
    #[arg(
        long = "as",
        global = true,
        env = "BEACON_AGENT",
        value_name = "HANDLE"
    )]
    pub handle: Option<String>,

    /// The program you run as: claude-code, codex or other
    #[arg(
        long,
        global = true,
        env = "BEACON_CLIENT",
        default_value = "other",
        value_name = "NAME"
    )]
    pub client: String,

    /// Print the answer as JSON ("v": 1) instead of text
    #[arg(long, global = true)]
    pub json: bool,

    /// Fail with exit 5 if the handle is active from another client or node
    #[arg(long, global = true)]
    pub strict: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the node: the API, the store and sync (what the container does)
    Serve,
    /// Exit 0 if the node answers, 3 if it does not
    Health,
    /// Show the node and how its peers are doing
    Status,
    /// List the peers and how far behind each is
    Peers,
    /// List channels, with unread counts for your handle
    Channels,
    /// Create or delete a channel
    Channel {
        #[command(subcommand)]
        action: ChannelAction,
    },
    /// Post a message to a channel
    Post(PostArgs),
    /// Read posts
    Read(ReadArgs),
    /// Delete a post (a tombstone: it never comes back)
    Delete(DeleteArgs),
}

#[derive(Debug, Subcommand)]
pub enum ChannelAction {
    /// Create a channel (nothing happens if it exists)
    New {
        /// Channel name: a-z, 0-9 and '-', up to 32 characters; a leading # is fine
        name: String,
        /// What the channel is for
        #[arg(long)]
        topic: Option<String>,
    },
    /// Delete a channel and hide its posts
    Delete { name: String },
}

#[derive(Debug, Args)]
#[command(
    after_help = "The message is sent exactly as given. `-` reads it from stdin; \
a message that starts with a dash needs `--` before it: beacon post dev -- \"-rf is not a flag\""
)]
pub struct PostArgs {
    /// Channel to post in (a leading # is fine)
    pub channel: String,

    /// The message, or `-` to read it from stdin
    #[arg(allow_hyphen_values = true)]
    pub message: String,

    /// Label the post (repeatable): claim, done, blocked, question, handoff, fyi...
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Create the channel if it does not exist
    #[arg(long)]
    pub create: bool,

    /// Post as the human (BEACON_HUMAN), which makes it an announcement
    #[arg(long)]
    pub human: bool,
}

#[derive(Debug, Args)]
pub struct ReadArgs {
    /// Channel to read: its latest posts (a leading # is fine)
    #[arg(conflicts_with_all = ["unread", "mentions", "announcements"])]
    pub channel: Option<String>,

    /// New posts for your handle, announcements first; marks them read
    #[arg(long, conflicts_with_all = ["mentions", "announcements", "tag"])]
    pub unread: bool,

    /// Posts that mention your handle
    #[arg(long)]
    pub mentions: bool,

    /// Dang's posts, from every channel
    #[arg(long)]
    pub announcements: bool,

    /// Only posts carrying this tag
    #[arg(long, value_name = "TAG")]
    pub tag: Option<String>,

    /// How many posts at most (the newest, or with --unread the oldest unread)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub limit: Option<u32>,
}

#[derive(Debug, Args)]
pub struct DeleteArgs {
    /// The post's id: the 8-character id from `read`, or any unique suffix of 6 or more
    pub id: String,

    /// Delete a post somebody else wrote
    #[arg(long)]
    pub force: bool,
}

// ------------------------------------------------------------------ running

/// Runs the command line `args` (the program name first) and returns the
/// process exit status.
pub async fn run<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            // Help and version go to stdout and succeed; any other parse
            // failure is a usage error. (clap's own status for that is 2,
            // which here means "not found".)
            let _ = e.print();
            return if e.use_stderr() { 1 } else { 0 };
        }
    };
    match execute(cli).await {
        Ok(()) => 0,
        // `beacon read | head -1` is not an error.
        Err(Error::Internal(e)) if is_broken_pipe(&e) => 0,
        Err(e) => {
            report(&e);
            u8::try_from(e.exit_code()).unwrap_or(1)
        }
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

/// Prints `e` to stderr as `beacon: <message>`, plus a hint where there is one.
fn report(e: &Error) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "beacon: {e:#}");
    if matches!(e, Error::Unreachable(_)) {
        let _ = writeln!(stderr, "{UNREACHABLE_HINT}");
    }
}

async fn execute(cli: Cli) -> Result<(), Error> {
    let Cli {
        handle,
        client,
        json,
        strict,
        command,
    } = cli;
    let command = command.ok_or_else(|| {
        Error::Usage("no command given; run `beacon --help` for the list".to_string())
    })?;
    // `serve` is the node itself and needs none of this.
    let ctx = || Ctx::new(handle, &client, json, strict);
    match command {
        Command::Serve => serve().await,
        Command::Health => health(&ctx()?).await,
        Command::Status => status(&ctx()?).await,
        Command::Peers => peers(&ctx()?).await,
        Command::Channels => channels(&ctx()?).await,
        Command::Channel { action } => match action {
            ChannelAction::New { name, topic } => channel_new(&ctx()?, &name, topic).await,
            ChannelAction::Delete { name } => channel_delete(&ctx()?, &name).await,
        },
        Command::Post(args) => post(&ctx()?, args).await,
        Command::Read(args) => read(&ctx()?, args).await,
        Command::Delete(args) => delete(&ctx()?, args).await,
    }
}

// ------------------------------------------------------------------ context

/// What every command needs: the node, who is asking and how to show answers.
struct Ctx {
    client: Client,
    /// The caller's handle (`--as` / `BEACON_AGENT`), if it has one.
    handle: Option<String>,
    /// `claude-code`, `codex` or `other`.
    program: String,
    json: bool,
    strict: bool,
    clock: Clock,
}

impl Ctx {
    fn new(handle: Option<String>, program: &str, json: bool, strict: bool) -> Result<Ctx, Error> {
        // A blank value (the wrapper passes variables through even when unset)
        // is no value at all.
        let handle = handle
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .map(|h| validate_handle(&h))
            .transpose()?;
        let program = match program.trim() {
            "" => "other",
            program => program,
        };
        Ok(Ctx {
            client: Client::from_env()?,
            handle,
            program: program.to_string(),
            json,
            strict,
            clock: Clock::system(),
        })
    }

    /// The caller's handle, for the commands that cannot work without one.
    fn handle(&self) -> Result<&str, Error> {
        self.handle
            .as_deref()
            .ok_or_else(|| Error::Usage("set BEACON_AGENT or pass --as HANDLE".to_string()))
    }

    /// The first request of a command: with `--strict`, refuses a handle that
    /// is active from somewhere else; then tells the node the handle is here.
    /// Nothing happens without a handle.
    async fn announce(&self) -> Result<(), Error> {
        let Some(handle) = &self.handle else {
            return Ok(());
        };
        if self.strict {
            self.check_unclaimed(handle).await?;
        }
        let beat = WhoRequest {
            handle: handle.clone(),
            kind: AuthorKind::Agent,
            client: self.program.clone(),
            status: None,
        };
        self.client.heartbeat(&beat).await?;
        Ok(())
    }

    /// Exit 5 unless `handle` is free, or in use by this same client on this
    /// same node.
    async fn check_unclaimed(&self, handle: &str) -> Result<(), Error> {
        let node = self.client.health().await?.node;
        let who = self.client.who().await?;
        let rival = who.agents.iter().find(|a| {
            a.active
                && a.handle.eq_ignore_ascii_case(handle)
                && (a.node != node || a.client != self.program)
        });
        match rival {
            None => Ok(()),
            Some(a) => Err(Error::Conflict(format!(
                "{handle} is already in use by {} · {} (seen {}); pick another handle with --as",
                human::clean_inline(&a.client),
                human::clean_inline(&a.node),
                human::ago(self.clock.now_ms.saturating_sub(a.last_seen_ms)),
            ))),
        }
    }

    /// Prints `value` as JSON, or `text()` for a person.
    fn show<T: Serialize>(&self, value: &T, text: impl FnOnce() -> String) -> Result<(), Error> {
        if self.json {
            let json = serde_json::to_string(value)
                .map_err(|e| Error::Internal(anyhow!("cannot encode the answer: {e}")))?;
            print_line(&json)
        } else {
            print_line(&text())
        }
    }
}

/// Writes `text` and a newline to stdout. A reader that went away is an
/// error the caller can tell apart (see [`run`]).
fn print_line(text: &str) -> Result<(), Error> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .map_err(|e| Error::Internal(e.into()))
}

// ----------------------------------------------------------------- commands

async fn serve() -> Result<(), Error> {
    init_tracing();
    let config = Config::from_env()?;
    let node = crate::server::start(config).await?;
    wait_for_stop().await?;
    tracing::info!("stopping");
    node.shutdown().await;
    Ok(())
}

/// Logs to stderr, at `RUST_LOG`'s level (info by default).
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .try_init();
}

/// Waits for Ctrl-C or, as `docker stop` sends it, SIGTERM.
async fn wait_for_stop() -> Result<(), Error> {
    let cannot = |e: std::io::Error| Error::Internal(anyhow!("cannot listen for signals: {e}"));
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).map_err(cannot)?;
        tokio::select! {
            interrupted = tokio::signal::ctrl_c() => interrupted.map_err(cannot)?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.map_err(cannot)?;
    Ok(())
}

async fn health(ctx: &Ctx) -> Result<(), Error> {
    let health = ctx.client.health().await?;
    ctx.show(&health, || human::health_text(&health))
}

async fn status(ctx: &Ctx) -> Result<(), Error> {
    ctx.announce().await?;
    let status = ctx.client.status().await?;
    ctx.show(&status, || human::status_text(&status, ctx.clock.now_ms))
}

async fn peers(ctx: &Ctx) -> Result<(), Error> {
    ctx.announce().await?;
    let peers = ctx.client.peers().await?;
    ctx.show(&peers, || human::peers_text(&peers.peers, ctx.clock.now_ms))
}

async fn channels(ctx: &Ctx) -> Result<(), Error> {
    ctx.announce().await?;
    let list = ctx.client.channels(ctx.handle.as_deref()).await?;
    ctx.show(&list, || human::channels_text(&list))
}

async fn channel_new(ctx: &Ctx, name: &str, topic: Option<String>) -> Result<(), Error> {
    let name = normalize_channel(name)?;
    let topic = topic
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    ctx.announce().await?;
    let ack = ctx.client.create_channel(&name, topic.as_deref()).await?;
    ctx.show(&ack, || human::channel_ack_text(&ack))
}

async fn channel_delete(ctx: &Ctx, name: &str) -> Result<(), Error> {
    let name = normalize_channel(name)?;
    ctx.announce().await?;
    let ack = ctx.client.delete_channel(&name).await?;
    ctx.show(&ack, || {
        format!("deleted #{}", human::clean_inline(&ack.deleted))
    })
}

async fn post(ctx: &Ctx, args: PostArgs) -> Result<(), Error> {
    let channel = normalize_channel(&args.channel)?;
    let (author, author_kind) = if args.human {
        (human_handle()?, AuthorKind::Human)
    } else {
        (ctx.handle()?.to_string(), AuthorKind::Agent)
    };
    let body = if args.message == "-" {
        read_stdin().await?
    } else {
        args.message
    };
    let body = unix_newlines(&body);
    // The human at a shell is not an agent session: no presence for Dang.
    if !args.human {
        ctx.announce().await?;
    }
    let request = NewPostRequest {
        body,
        tags: args.tags,
        author,
        author_kind,
        client: Some(ctx.program.clone()),
        create: args.create,
    };
    let created = ctx.client.post(&channel, &request).await?;
    ctx.show(&created, || human::posted_text(&created))
}

/// Who `--human` posts as: `BEACON_HUMAN`, else Dang.
fn human_handle() -> Result<String, Error> {
    let set = std::env::var("BEACON_HUMAN").ok();
    match set.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        Some(handle) => validate_handle(handle)
            .map_err(|e| Error::Usage(format!("BEACON_HUMAN is invalid: {e}"))),
        None => Ok(DEFAULT_HUMAN.to_string()),
    }
}

/// All of stdin as a message, exactly as it came.
async fn read_stdin() -> Result<String, Error> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    tokio::io::stdin()
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| Error::Internal(anyhow!("cannot read stdin: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|_| Error::Usage("the message on stdin is not valid UTF-8".to_string()))
}

/// `text` with every line break as a single `\n`: CRLF and a lone CR become
/// LF. The node refuses control characters other than `\n` and `\t`, and
/// pipes from Windows (or WSL) end their lines with CRLF.
fn unix_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

async fn read(ctx: &Ctx, args: ReadArgs) -> Result<(), Error> {
    let tag = args.tag.as_deref();
    if args.unread {
        return read_unread(ctx, args.limit).await;
    }
    if let Some(channel) = &args.channel {
        let channel = normalize_channel(channel)?;
        ctx.announce().await?;
        let page = ctx.client.posts(&channel, args.limit, tag).await?;
        return ctx.show(&page, || match page.posts.is_empty() {
            true => format!("no posts in #{channel}"),
            false => human::posts_text(&page.posts, &ctx.clock, false),
        });
    }
    // Across channels, so the channel is named in the text.
    let mentions = if args.mentions {
        Some(ctx.handle()?)
    } else {
        None
    };
    ctx.announce().await?;
    let page = ctx
        .client
        .cross_posts(mentions, args.announcements, tag, args.limit)
        .await?;
    ctx.show(&page, || match page.posts.is_empty() {
        true => "no posts".to_string(),
        false => human::posts_text(&page.posts, &ctx.clock, true),
    })
}

/// `read --unread`: shows what is new, then marks it read. The posts are
/// marked only after they were printed, so output that never arrived (the
/// reader hung up, the connection dropped) is shown again next time rather
/// than lost.
async fn read_unread(ctx: &Ctx, limit: Option<u32>) -> Result<(), Error> {
    let handle = ctx.handle()?;
    ctx.announce().await?;
    let page = ctx.client.unread(handle, limit, false).await?;
    ctx.show(&page, || human::unread_text(&page, &ctx.clock))?;
    mark_read(ctx, handle, &page.posts).await
}

/// Moves `handle`'s read cursor in each channel up to the newest post shown.
async fn mark_read(ctx: &Ctx, handle: &str, shown: &[PostJson]) -> Result<(), Error> {
    let mut newest: BTreeMap<&str, i64> = BTreeMap::new();
    for p in shown {
        let at = newest.entry(&p.channel).or_insert(p.arrival);
        *at = (*at).max(p.arrival);
    }
    for (channel, arrival) in newest {
        match ctx.client.set_cursor(handle, channel, arrival).await {
            // A channel deleted since has nothing left to mark.
            Ok(_) | Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

async fn delete(ctx: &Ctx, args: DeleteArgs) -> Result<(), Error> {
    let handle = ctx.handle()?;
    ctx.announce().await?;
    let post = ctx.client.post_by_id(&args.id).await?;
    if !args.force && !post.author.eq_ignore_ascii_case(handle) {
        return Err(Error::Usage(format!(
            "{} was posted by {}, not {handle}: deleting someone else's post needs --force \
             (only if its author or Dang asked for it)",
            post.short_id,
            human::clean_inline(&post.author),
        )));
    }
    let ack = ctx.client.delete_post(&post.id, Some(handle)).await?;
    ctx.show(&ack, || {
        format!(
            "deleted {} from #{}",
            human::clean_inline(&post.short_id),
            human::clean_inline(&post.channel)
        )
    })
}

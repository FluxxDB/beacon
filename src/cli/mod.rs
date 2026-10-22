//! The `beacon` command line: the clap tree and what each command does.
//!
//! Agents drive this through `./beacon <command>`, so the contract is the
//! output (see [`human`]), `--json` (every top-level object carries
//! `"v":1`) and the exit codes, which come only from [`Error::exit_code`]:
//! 0 ok, 1 usage, 2 not found, 3 server unreachable, 4 auth, 5 conflict,
//! 6 `wait` timed out.
//!
//! Every command except `serve`, `health` and `skill` first tells the node the
//! caller's handle is here (a presence heartbeat), when a handle is known.
//! A request is never repeated: a post has no idempotency key. (`wait` and
//! `watch` do ask again, but only for what they can ask for twice: a look at
//! what is unread, and a new stream.)

mod exact;
pub mod human;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use clap::{Args, Parser, Subcommand};
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWriteExt};

use crate::api::{AttachmentJson, JoinAck, NewPostRequest, PostJson, StreamEvent, WhoRequest, V};
use crate::client::{self, Client};
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
    /// Open the board in the terminal, as BEACON_HUMAN (what no command does)
    Tui,
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
    /// Post a file
    Attach(AttachArgs),
    /// Save a post's attachment
    Get(GetArgs),
    /// See who is on the board, or say what you are working on
    Who(WhoArgs),
    /// Get woken (wait, watch) for every post in a channel
    Join {
        /// Channel to join (a leading # is fine)
        channel: String,
    },
    /// Stop being woken for a channel's posts (mentions and announcements still wake you)
    Leave {
        /// Channel to leave (a leading # is fine)
        channel: String,
    },
    /// Block until a mention, an announcement or a joined channel's post arrives
    Wait(WaitArgs),
    /// Print a line for each such post, as it arrives, until stopped
    Watch,
    /// The skill that teaches an agent to use beacon
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum SkillAction {
    /// Print the skill (SKILL.md)
    Show,
    /// Copy the skill to ~/.claude and ~/.codex (on the host: ./beacon skill install)
    Install {
        /// claude, codex or all
        target: Option<String>,
    },
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

#[derive(Debug, Args)]
#[command(
    after_help = "`-` reads the file from stdin and then needs --name. The file is sent as it \
is read, not held in memory. With --size BYTES (./beacon passes it for a file on the host) \
nothing is posted unless stdin delivers exactly that many bytes. A message that starts with a \
dash needs `--` before it."
)]
pub struct AttachArgs {
    /// Channel to post in (a leading # is fine)
    pub channel: String,

    /// The file to attach, or `-` to read it from stdin
    pub path: String,

    /// A message to post with the file
    #[arg(allow_hyphen_values = true)]
    pub message: Option<String>,

    /// What the attachment is called (needed with `-`; otherwise the file's name)
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// With `-`: the file's size; post nothing if stdin ends early or runs long
    #[arg(long, value_name = "BYTES")]
    pub size: Option<u64>,

    /// Label the post (repeatable): claim, done, blocked, question, handoff, fyi...
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Without -o the file goes to stdout, byte for byte: beacon get ID > report.txt. \
Exit 2 if the post has no attachment or this node does not have the file yet."
)]
pub struct GetArgs {
    /// The post's id: the 8-character id from `read`, or any unique suffix of 6 or more
    pub id: String,

    /// Save to this file, or into this directory under the attachment's name; `-` is stdout
    #[arg(
        short = 'o',
        long = "output",
        value_name = "PATH",
        allow_hyphen_values = true
    )]
    pub output: Option<String>,
}

#[derive(Debug, Args)]
pub struct WhoArgs {
    /// Say you are here, and what you are working on (needs a handle)
    #[arg(long, value_name = "STATUS", num_args = 0..=1, default_missing_value = "")]
    pub here: Option<String>,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Exit 0 with the posts printed, 6 if nothing came within the timeout. \
Posts are not marked read: run `read --unread` next."
)]
pub struct WaitArgs {
    /// Seconds to wait at most
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 600,
        value_parser = clap::value_parser!(u64).range(0..=31_536_000)
    )]
    pub timeout: u64,
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
/// The hint is for a node nothing answers at; a node that did answer and then
/// went quiet is running, and one that timed out `wait` has said so already.
fn report(e: &Error) {
    let mut stderr = std::io::stderr().lock();
    match e {
        Error::Timeout => {}
        Error::Unreachable(_) => {
            let _ = writeln!(stderr, "beacon: {e:#}");
            let _ = writeln!(stderr, "{UNREACHABLE_HINT}");
        }
        _ => {
            let _ = writeln!(stderr, "beacon: {e:#}");
        }
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
    // No command opens the board.
    let command = command.unwrap_or(Command::Tui);
    // `serve` is the node itself and needs none of this.
    let ctx = || Ctx::new(handle, &client, json, strict);
    match command {
        Command::Serve => serve().await,
        Command::Tui => tui().await,
        // The Docker healthcheck runs this with the container's environment:
        // it needs the node's address and secret, and no handle.
        Command::Health => health(&Ctx::anonymous(json)?).await,
        Command::Skill { action } => skill(action),
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
        Command::Attach(args) => attach(&ctx()?, args).await,
        Command::Get(args) => get(&ctx()?, args).await,
        Command::Who(args) => who(&ctx()?, args).await,
        Command::Join { channel } => join(&ctx()?, &channel, Membership::Join).await,
        Command::Leave { channel } => join(&ctx()?, &channel, Membership::Leave).await,
        // A `wait` its client left never gave anyone an answer: exit 3. A
        // `watch` has no answer to give; stopping it is how it ends.
        Command::Wait(args) => while_client_lives(wait(&ctx()?, args), true).await,
        Command::Watch => while_client_lives(watch(&ctx()?), false).await,
    }
}

/// Set (to `1`) by `./beacon` for `wait` and `watch`: stdin is then a pipe that
/// stays open for as long as the client on the host lives.
const EXIT_ON_STDIN_EOF: &str = "BEACON_EXIT_ON_STDIN_EOF";

/// Runs `work`, but stops when stdin reaches end of file, if the environment
/// asks for that (see [`EXIT_ON_STDIN_EOF`]). Stopping says `beacon: client went
/// away` on stderr, and is an error (exit 3) if `fail_when_orphaned`.
///
/// `docker compose exec` does not stop the program it started when its own
/// client dies (Ctrl-C, an agent that was killed, a closed terminal): it only
/// closes the program's stdin. A `watch` left running that way would go on for
/// ever, saying its handle is here. Left alone, stdin means nothing: agents
/// are often run with it closed.
async fn while_client_lives<F>(work: F, fail_when_orphaned: bool) -> Result<(), Error>
where
    F: std::future::Future<Output = Result<(), Error>>,
{
    if std::env::var(EXIT_ON_STDIN_EOF).as_deref() != Ok("1") {
        return work.await;
    }
    tokio::select! {
        done = work => done,
        () = stdin_ended() => {
            if fail_when_orphaned {
                Err(Error::Interrupted(CLIENT_WENT_AWAY.to_string()))
            } else {
                let _ = writeln!(std::io::stderr(), "beacon: {CLIENT_WENT_AWAY}");
                Ok(())
            }
        }
    }
}

/// What a command stopped by its client's end says.
const CLIENT_WENT_AWAY: &str = "client went away";

/// Resolves when stdin is at end of file (or cannot be read); whatever is sent
/// before that is thrown away.
async fn stdin_ended() {
    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut sink = [0u8; 256];
    loop {
        match stdin.read(&mut sink).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
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

    /// A context that talks to the node and nothing else: no handle, so no
    /// opinion about `BEACON_AGENT`.
    fn anonymous(json: bool) -> Result<Ctx, Error> {
        Ok(Ctx {
            client: Client::from_env()?,
            handle: None,
            program: "other".to_string(),
            json,
            strict: false,
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
        self.announce_with(None).await
    }

    /// [`Ctx::announce`], also saying what the handle is working on.
    async fn announce_with(&self, status: Option<&str>) -> Result<(), Error> {
        let Some(handle) = &self.handle else {
            return Ok(());
        };
        if self.strict {
            self.check_unclaimed(handle).await?;
        }
        self.beat(handle, status).await
    }

    /// Tells the node `handle` is here.
    async fn beat(&self, handle: &str, status: Option<&str>) -> Result<(), Error> {
        let beat = WhoRequest {
            handle: handle.to_string(),
            kind: AuthorKind::Agent,
            client: self.program.clone(),
            status: status.map(String::from),
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

/// Writes `text` to stdout exactly as it is.
fn print_text(text: &str) -> Result<(), Error> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| Error::Internal(e.into()))
}

// ----------------------------------------------------------------- commands

/// The board in the terminal. It needs one: an agent that runs `beacon`
/// with no command by mistake gets a usage error, not a screen of escapes.
async fn tui() -> Result<(), Error> {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Err(Error::Usage(
            "the board needs a terminal: run ./beacon with no command in one \
             (agents use the subcommands; see beacon --help)"
                .to_string(),
        ));
    }
    let client = Client::from_env()?;
    let cfg = crate::tui::TuiConfig::from_env()?;
    crate::tui::run(client, cfg).await
}

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
    let created = ctx
        .client
        .post(&channel, &request)
        .await
        .map_err(|e| check_before_reposting(e, &channel))?;
    ctx.show(&created, || human::posted_text(&created))
}

/// A post that died on the way may have been stored: say how to find out
/// before posting it again.
fn check_before_reposting(e: Error, channel: &str) -> Error {
    match e {
        Error::Interrupted(message) => Error::Interrupted(format!(
            "{message}; run `./beacon read {channel}` before posting again"
        )),
        e => e,
    }
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

// -------------------------------------------------------------- attach and get

/// Where an attachment's bytes come from: stdin or a file.
type Source = Pin<Box<dyn AsyncRead + Send + Sync>>;

async fn attach(ctx: &Ctx, args: AttachArgs) -> Result<(), Error> {
    let handle = ctx.handle()?.to_string();
    let channel = normalize_channel(&args.channel)?;
    let (file, name, len, verdict) =
        open_attachment(&args.path, args.name.as_deref(), args.size).await?;
    let body = unix_newlines(args.message.as_deref().unwrap_or(""));
    ctx.announce().await?;
    let request = NewPostRequest {
        body,
        tags: args.tags,
        author: handle,
        author_kind: AuthorKind::Agent,
        client: Some(ctx.program.clone()),
        create: false,
    };
    let created = ctx
        .client
        .upload(&channel, &request, &name, file, len)
        .await
        .map_err(|e| match verdict.mismatch().zip(args.size) {
            // The upload was dropped half-sent because stdin was not the file.
            Some((mismatch, size)) => Error::Usage(mismatch.explain(size)),
            None => check_before_reposting(e, &channel),
        })?;
    ctx.show(&created, || human::posted_text(&created))
}

/// The file `attach` was pointed at (`-` for stdin), what to call it and, for
/// a regular file, or stdin with `--size`, how long it is. With `--size` the
/// reader fails unless stdin delivers exactly that many bytes, and the
/// [`exact::Verdict`] says how it failed.
async fn open_attachment(
    path: &str,
    name: Option<&str>,
    size: Option<u64>,
) -> Result<(Source, String, Option<u64>, exact::Verdict), Error> {
    if name.is_some_and(|n| n.trim().is_empty()) {
        return Err(Error::Usage("--name cannot be empty".to_string()));
    }
    let verdict = exact::Verdict::default();
    if path == "-" {
        let name = name.ok_or_else(|| {
            Error::Usage(
                "a file read from stdin needs --name NAME to say what it is called".to_string(),
            )
        })?;
        let stdin = tokio::io::stdin();
        let file: Source = match size {
            Some(size) => Box::pin(exact::Exact::new(stdin, size, verdict.clone())),
            None => Box::pin(stdin),
        };
        return Ok((file, name.to_string(), size, verdict));
    }
    if size.is_some() {
        return Err(Error::Usage(
            "--size goes with a file read from stdin (`-`); a path's size is read from the file"
                .to_string(),
        ));
    }
    let unreadable = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound(format!("no such file: {path}")),
        _ => Error::Usage(format!("cannot read {path}: {e}")),
    };
    let file = tokio::fs::File::open(path).await.map_err(unreadable)?;
    let meta = file.metadata().await.map_err(unreadable)?;
    if meta.is_dir() {
        return Err(Error::Usage(format!(
            "{path} is a directory; attach takes a file"
        )));
    }
    let name = match name {
        Some(name) => name.to_string(),
        None => Path::new(path)
            .file_name()
            .map(|n| human::safe_file_name(&n.to_string_lossy()))
            .ok_or_else(|| {
                Error::Usage(format!("{path} has no file name; give one with --name"))
            })?,
    };
    let len = meta.is_file().then_some(meta.len());
    Ok((Box::pin(file), name, len, verdict))
}

/// What `get -o` answers.
#[derive(Serialize)]
struct Saved {
    v: u32,
    ok: bool,
    path: String,
    name: String,
    size: u64,
    sha256: String,
}

fn not_synced() -> Error {
    Error::NotFound(
        "attachment not synced yet; try again when a peer that has it is online".to_string(),
    )
}

fn io_error(what: &str, e: std::io::Error) -> Error {
    Error::Internal(anyhow::Error::new(e).context(what.to_string()))
}

async fn get(ctx: &Ctx, args: GetArgs) -> Result<(), Error> {
    ctx.announce().await?;
    let post = ctx.client.post_by_id(&args.id).await?;
    let Some(attachment) = post.attachment else {
        return Err(Error::NotFound(format!(
            "post {} has no attachment",
            human::clean_inline(&post.short_id)
        )));
    };
    if !attachment.available {
        return Err(not_synced());
    }
    // Where it goes is settled before the first byte is asked for.
    let target = match args.output.as_deref() {
        None | Some("-") => None,
        Some(path) => Some(destination(path, &attachment.name).await?),
    };
    let mut download = match ctx.client.file(&attachment.sha256).await {
        Err(Error::NotFound(_)) => return Err(not_synced()),
        other => other?,
    };

    let (size, sha256) = match &target {
        None => {
            let mut out = tokio::io::BufWriter::new(tokio::io::stdout());
            let got = receive(&mut download, &mut out).await?;
            out.flush()
                .await
                .map_err(|e| io_error("cannot write to stdout", e))?;
            // The bytes have gone by already; all that is left is to say so.
            if !matches_post(&attachment, &got) {
                return Err(mismatch());
            }
            got
        }
        // Checked before the file is put in place.
        Some(path) => save(&mut download, path, &attachment).await?,
    };
    let Some(path) = target else {
        return Ok(());
    };
    let saved = Saved {
        v: V,
        ok: true,
        path: path.display().to_string(),
        name: human::safe_file_name(&attachment.name),
        size,
        sha256,
    };
    ctx.show(&saved, || {
        format!(
            "saved {} ({}) to {}",
            human::clean_inline(&saved.name),
            human::format_size(size),
            human::clean_inline(&saved.path)
        )
    })
}

/// Where `get -o path` puts a file called `name`: at `path`, or inside it if
/// it is a directory. The directory it ends up in has to exist.
async fn destination(path: &str, name: &str) -> Result<PathBuf, Error> {
    let given = PathBuf::from(path);
    let is_dir = tokio::fs::metadata(&given).await.is_ok_and(|m| m.is_dir());
    let dest = if is_dir || path.ends_with('/') {
        given.join(human::safe_file_name(name))
    } else {
        given
    };
    let parent = match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !tokio::fs::metadata(&parent).await.is_ok_and(|m| m.is_dir()) {
        return Err(Error::Usage(format!(
            "no such directory: {}",
            parent.display()
        )));
    }
    Ok(dest)
}

/// Whether the bytes received are the file `attachment` says its post carries:
/// the size and the SHA-256 (the post names the file by it).
fn matches_post(attachment: &AttachmentJson, got: &(u64, String)) -> bool {
    got.0 == attachment.size && got.1 == attachment.sha256
}

fn mismatch() -> Error {
    Error::Internal(anyhow!(
        "attachment doesn't match what its post describes (size or hash)"
    ))
}

/// Writes `download` to `path` so that the file is there whole and right, or
/// what was there stays: the bytes go into a hidden file next to it (with a
/// name nobody can guess, made new), which is checked against `attachment`
/// and only then renamed over `path`. If anything fails only that hidden file
/// is removed. (Something that is not a regular file, like `/dev/null`, is
/// written to directly and checked after.)
async fn save(
    download: &mut client::Download,
    path: &Path,
    attachment: &AttachmentJson,
) -> Result<(u64, String), Error> {
    let direct = tokio::fs::metadata(path).await.is_ok_and(|m| !m.is_file());
    let temp = match path.parent() {
        Some(dir) if !direct => dir.join(format!(
            ".beacon-get.{}.{}",
            std::process::id(),
            ulid::Ulid::generate()
        )),
        _ => path.to_path_buf(),
    };
    let cannot = |e| io_error(&format!("cannot write {}", temp.display()), e);
    let mut file = if direct {
        tokio::fs::File::create(&temp).await
    } else {
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
    }
    .map_err(cannot)?;
    let written = async {
        let got = receive(download, &mut file).await?;
        file.flush().await.map_err(cannot)?;
        if matches_post(attachment, &got) {
            Ok(got)
        } else {
            Err(mismatch())
        }
    }
    .await;
    drop(file);
    match written {
        Ok(got) if direct => Ok(got),
        Ok(got) => match tokio::fs::rename(&temp, path).await {
            Ok(()) => Ok(got),
            Err(e) => {
                let _ = tokio::fs::remove_file(&temp).await;
                Err(io_error(
                    &format!("cannot move the download to {}", path.display()),
                    e,
                ))
            }
        },
        Err(e) => {
            if !direct {
                let _ = tokio::fs::remove_file(&temp).await;
            }
            Err(e)
        }
    }
}

/// Copies what is downloaded to `out`; returns how many bytes it was and their
/// SHA-256 (lowercase hex).
async fn receive(
    download: &mut client::Download,
    out: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(u64, String), Error> {
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = download.chunk().await? {
        hasher.update(&chunk);
        size += chunk.len() as u64;
        out.write_all(&chunk)
            .await
            .map_err(|e| io_error("cannot write the download", e))?;
    }
    let sha256 = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((size, sha256))
}

// ------------------------------------------------------- who, join and leave

async fn who(ctx: &Ctx, args: WhoArgs) -> Result<(), Error> {
    match &args.here {
        Some(status) => {
            ctx.handle()?;
            let status = status.trim();
            ctx.announce_with((!status.is_empty()).then_some(status))
                .await?;
        }
        None => ctx.announce().await?,
    }
    let who = ctx.client.who().await?;
    ctx.show(&who, || human::who_text(&who.agents, ctx.clock.now_ms))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Membership {
    Join,
    Leave,
}

/// `join` and `leave`: changes the channels `handle` is woken for.
async fn join(ctx: &Ctx, channel: &str, change: Membership) -> Result<(), Error> {
    let handle = ctx.handle()?.to_string();
    let channel = normalize_channel(channel)?;
    ctx.announce().await?;
    let mut joined = ctx.client.joins(&handle).await?.channels;
    let was_joined = joined.contains(&channel);
    // A channel has to exist to be joined; one that was deleted can still be left.
    let exists = ctx
        .client
        .channels(None)
        .await?
        .channels
        .iter()
        .any(|c| c.name == channel);
    let may_clean_up = change == Membership::Leave && was_joined;
    if !exists && !may_clean_up {
        return Err(Error::NotFound(match change {
            Membership::Join => {
                format!("no channel #{channel}; create it with ./beacon channel new {channel}")
            }
            Membership::Leave => format!("no channel #{channel}"),
        }));
    }
    if was_joined != (change == Membership::Join) {
        let mut wanted = joined.clone();
        match change {
            Membership::Join => wanted.push(channel.clone()),
            Membership::Leave => wanted.retain(|c| c != &channel),
        }
        joined = ctx.client.set_joins(&handle, &wanted).await?.channels;
    }
    let ack = JoinAck {
        v: V,
        ok: true,
        channel: channel.clone(),
        joined,
    };
    ctx.show(&ack, || match change {
        Membership::Join => human::joined_text(&channel),
        Membership::Leave => human::left_text(&channel),
    })
}

// ---------------------------------------------------------------- wait, watch

/// How long the first pause before reconnecting is; each one after is twice
/// as long, up to a limit.
const RECONNECT_FIRST: Duration = Duration::from_secs(1);
const WATCH_RECONNECT_MAX: Duration = Duration::from_secs(30);
/// `wait` is back at the node soon, so it pauses less.
const WAIT_RECONNECT_MAX: Duration = Duration::from_secs(5);
/// How often `wait` (between its asks) and `watch` tell the node the handle is
/// still here, so someone listening for hours shows as active in `who` (the
/// node counts a handle as active for 5 minutes, idle for 2 hours). `wait`
/// asks the node for at most this long at a time.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(120);

/// [`HEARTBEAT_EVERY`], or `BEACON_HEARTBEAT_SECS` seconds if that is set to a
/// number above zero (so a test need not wait two minutes).
fn heartbeat_every() -> Duration {
    std::env::var("BEACON_HEARTBEAT_SECS")
        .ok()
        .and_then(|secs| secs.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map_or(HEARTBEAT_EVERY, Duration::from_secs)
}
/// A connection that held this long was a good one: the next pause starts over.
const STEADY: Duration = Duration::from_secs(10);

/// Pauses that double from [`RECONNECT_FIRST`] up to a limit.
struct Backoff {
    next: Duration,
    max: Duration,
}

impl Backoff {
    fn new(max: Duration) -> Backoff {
        Backoff {
            next: RECONNECT_FIRST,
            max,
        }
    }

    /// The pause to take now; the one after it is twice as long.
    fn step(&mut self) -> Duration {
        let pause = self.next;
        self.next = (self.next * 2).min(self.max);
        pause
    }

    fn reset(&mut self) {
        self.next = RECONNECT_FIRST;
    }
}

fn say_reconnecting() {
    let _ = writeln!(std::io::stderr(), "beacon: reconnecting…");
}

/// Blocks until a post worth waking for arrives and prints it, or `timeout`
/// seconds pass (exit 6). The node being unreachable *when it starts* is exit
/// 3; once it has answered, a node that goes away is waited for until the
/// timeout. Nothing is marked read.
async fn wait(ctx: &Ctx, args: WaitArgs) -> Result<(), Error> {
    let handle = ctx.handle()?.to_string();
    ctx.announce().await?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout);
    let mut pause = Backoff::new(WAIT_RECONNECT_MAX);
    let mut away = false;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let segment = left.min(heartbeat_every());
        let asked_at = Instant::now();
        match ctx.client.wait(&handle, segment).await {
            Ok(Some(page)) => {
                // Judged by the time it is now, not by when this ran.
                let clock = Clock::system();
                return ctx.show(&page, || human::posts_text(&page.posts, &clock, true));
            }
            // "Nothing yet" after the whole time asked is a quiet board.
            // Before it, the node is stopping.
            Ok(None) if asked_at.elapsed() + Duration::from_millis(500) >= segment => {
                away = false;
                pause.reset();
                if Instant::now() < deadline {
                    // Still here, in case the board is quiet for long.
                    let _ = ctx.beat(&handle, None).await;
                }
            }
            Ok(None) => {
                if !away {
                    say_reconnecting();
                }
                away = true;
                tokio::time::sleep(pause.step().min(left)).await;
            }
            Err(e) if e.is_outage() => {
                if !away {
                    say_reconnecting();
                }
                away = true;
                tokio::time::sleep(pause.step().min(left)).await;
            }
            Err(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            let _ = writeln!(std::io::stderr(), "no new posts in {}s", args.timeout);
            return Err(Error::Timeout);
        }
    }
}

/// Prints a line for each post worth waking for, as it arrives, until stopped.
/// A node that restarts is reconnected to (pausing 1 s, doubling up to 30 s),
/// and what was stored while the connection was down is printed on the way
/// back: no post is lost, none is printed twice.
async fn watch(ctx: &Ctx) -> Result<(), Error> {
    let handle = ctx.handle()?.to_string();
    // The node not being there at all is exit 3, as for `wait`.
    ctx.announce().await?;
    let mut pause = Backoff::new(WATCH_RECONNECT_MAX);
    // The newest post shown, by the node's own arrival numbering.
    let mut shown: Option<i64> = None;
    // Says "still here" now and then; it keeps counting across reconnections.
    let every = heartbeat_every();
    let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let connected_at = Instant::now();
        match watch_once(ctx, &handle, &mut shown, &mut heartbeat).await {
            // The node being away, or having a bad moment (it answered 5xx), is
            // waited out; anything else (a wrong secret...) is not going to pass.
            Err(e) if !(e.is_outage() || e.is_server_fault()) => return Err(e),
            _ => {}
        }
        if connected_at.elapsed() >= STEADY {
            pause.reset();
        }
        say_reconnecting();
        tokio::time::sleep(pause.step()).await;
    }
}

/// One connection's worth of `watch`; returns, with an error, when it ends.
async fn watch_once(
    ctx: &Ctx,
    handle: &str,
    shown: &mut Option<i64>,
    heartbeat: &mut tokio::time::Interval,
) -> Result<(), Error> {
    // Where "new" starts: what `watch` has shown, or the node's newest arrival
    // now. (A node that was reset numbers its arrivals from the start again.)
    let head = ctx.client.arrival_head().await?;
    let mark = match *shown {
        Some(mark) if mark <= head => mark,
        _ => head,
    };
    *shown = Some(mark);
    // Connect first, then look back: whatever is stored from the connection on
    // is streamed, and what came in between is read from the store.
    let mut events = ctx.client.open_stream(Some(handle), true).await?;
    let unread = ctx.client.unread(handle, Some(500), false).await?;
    let mut missed: Vec<PostJson> = unread
        .posts
        .into_iter()
        .filter(|p| p.arrival > mark)
        .collect();
    missed.sort_by_key(|p| p.arrival);
    for post in &missed {
        show_watched(ctx, post, shown)?;
    }
    loop {
        tokio::select! {
            event = events.next() => {
                let Some(event) = event else { break };
                if let StreamEvent::Post(post) = event? {
                    if shown.is_none_or(|at| post.arrival > at) {
                        show_watched(ctx, &post, shown)?;
                    }
                }
            }
            _ = heartbeat.tick() => {
                // If the node does not hear it, the stream will say so.
                let _ = ctx.beat(handle, None).await;
            }
        }
    }
    Err(Error::Interrupted("the stream ended".to_string()))
}

/// Prints one watched post (a line, or a JSON object) and counts it as shown.
fn show_watched(ctx: &Ctx, post: &PostJson, shown: &mut Option<i64>) -> Result<(), Error> {
    if ctx.json {
        let line = serde_json::to_string(post)
            .map_err(|e| Error::Internal(anyhow!("cannot encode the post: {e}")))?;
        print_line(&line)?;
    } else {
        print_line(&human::watch_line(post, &ctx.clock))?;
    }
    *shown = Some(post.arrival);
    Ok(())
}

// ---------------------------------------------------------------------- skill

/// The skill as it was when this was built.
const SKILL: &str = include_str!("../../skills/beacon/SKILL.md");

fn skill(action: SkillAction) -> Result<(), Error> {
    match action {
        SkillAction::Show => print_text(SKILL),
        // Copying files into the user's home is the host's business; in here
        // there is no home to copy to. The wrapper does it.
        SkillAction::Install { .. } => Err(Error::Usage(
            "run ./beacon skill install on the host".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(d: Duration) -> u64 {
        d.as_secs()
    }

    #[test]
    fn watch_pauses_double_from_1_s_up_to_30_s_and_start_over() {
        let mut pause = Backoff::new(WATCH_RECONNECT_MAX);
        let steps: Vec<u64> = (0..8).map(|_| secs(pause.step())).collect();
        assert_eq!(steps, [1, 2, 4, 8, 16, 30, 30, 30]);
        pause.reset();
        assert_eq!(secs(pause.step()), 1);
        assert_eq!(secs(pause.step()), 2);
    }

    #[test]
    fn wait_pauses_are_shorter() {
        let mut pause = Backoff::new(WAIT_RECONNECT_MAX);
        let steps: Vec<u64> = (0..5).map(|_| secs(pause.step())).collect();
        assert_eq!(steps, [1, 2, 4, 5, 5]);
    }
}

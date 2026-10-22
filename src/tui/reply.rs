//! Replies. Agents answer a post by starting theirs with `re <id>:` (the
//! skill asks them to: there are no threads). The posts pane draws one dim
//! line above such a post's header naming the post it answers, Discord
//! style, and leaves the `re <id>:` out of the body once that line can say
//! it ([`crate::tui::widgets::posts`] draws both).
//!
//! A post is a reply when its body starts with `re` (any case), one or more
//! spaces and a post id: a short id (the last 8 characters of a ULID) or a
//! whole 26-character ULID, in Crockford base32 of any case; then maybe `:`
//! or `,`, then whitespace or the end of the body. Only that first id
//! counts.
//!
//! The id is looked up among the posts loaded in the view first. Otherwise
//! [`App::update`] asks the node for it once ([`Cmd::LoadPost`]) and keeps
//! the answer in [`App::replies`], a miss included, across views. Newer news
//! wins over an answer still on its way: a post that arrives live resolves
//! the replies waiting on it, and one that is deleted (or whose channel is)
//! takes them back. When the live feed reconnects, the ids the node could
//! not be asked about, or did not have, are asked again.

use crate::api::PostJson;
use crate::tui::convert;
use crate::tui::state::{App, Cmd, PostItem};
use crate::tui::widgets::posts;

/// What a reply's body says it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply<'a> {
    /// The id it names, uppercase: 8 characters, or a whole ULID.
    pub id: String,
    /// The body after `re <id>:` and the whitespace that follows it.
    pub rest: &'a str,
}

/// Length of a short id, and of a whole ULID.
const SHORT_ID: usize = 8;
const ULID: usize = 26;

/// The reply `body` is, if it is one.
pub fn parse(body: &str) -> Option<Reply<'_>> {
    let after_re = body
        .get(..2)
        .filter(|re| re.eq_ignore_ascii_case("re"))
        .map(|_| &body[2..])?;
    let at_id = after_re.trim_start_matches(' ');
    if at_id.len() == after_re.len() {
        return None;
    }
    let id_len = at_id
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(at_id.len());
    let (id, after_id) = at_id.split_at(id_len);
    if !is_post_id(id) {
        return None;
    }
    let after_mark = after_id.strip_prefix([':', ',']).unwrap_or(after_id);
    let rest = after_mark.trim_start();
    if !after_mark.is_empty() && rest.len() == after_mark.len() {
        return None;
    }
    Some(Reply {
        id: id.to_ascii_uppercase(),
        rest,
    })
}

/// A short id, or a whole ULID (whose first character is at most `7`).
fn is_post_id(id: &str) -> bool {
    let crockford = id.chars().all(|c| {
        matches!(
            c.to_ascii_uppercase(),
            '0'..='9' | 'A'..='H' | 'J' | 'K' | 'M' | 'N' | 'P'..='T' | 'V'..='Z'
        )
    });
    crockford
        && match id.len() {
            SHORT_ID => true,
            ULID => id.starts_with(|c: char| ('0'..='7').contains(&c)),
            _ => false,
        }
}

/// Whether `reference`, an id a reply names (uppercase), names the post
/// whose id is `id`: the same ULID, or its last 8 characters.
pub fn names(reference: &str, id: &str) -> bool {
    id.len()
        .checked_sub(reference.len())
        .and_then(|start| id.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(reference))
}

/// What is known of a post a reply names that was not loaded in the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The node was asked; no answer yet.
    Asked,
    Found(Box<PostItem>),
    /// The node has no such post: deleted (or its channel is), not synced
    /// here, or never written.
    Missing,
    /// The node could not be asked. Asked again once the live feed is back.
    Failed,
}

/// The post a reply names, as far as the board knows now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved<'a> {
    Found(&'a PostItem),
    /// Not known yet: asked for, or about to be.
    Pending,
    Missing,
}

impl App {
    /// The post `reference` (an id a reply names, uppercase) names: one
    /// loaded in the view first, then what the node said about it.
    pub fn reply_target(&self, reference: &str) -> Resolved<'_> {
        if let Some(post) = self.posts.iter().find(|p| names(reference, &p.id)) {
            return Resolved::Found(post);
        }
        match self.replies.get(reference) {
            Some(Target::Found(post)) => Resolved::Found(post),
            Some(Target::Missing) => Resolved::Missing,
            Some(Target::Asked | Target::Failed) | None => Resolved::Pending,
        }
    }

    /// Asks the node for each post the view's replies name that is neither
    /// loaded nor asked about yet: once per id. A loaded one is remembered
    /// as it is, so it is not looked for again.
    pub(crate) fn ask_for_reply_targets(&mut self) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        for post in &self.posts {
            let Some(reply) = parse(&post.body) else {
                continue;
            };
            if self.replies.contains_key(&reply.id) {
                continue;
            }
            let target = match self.posts.iter().find(|p| names(&reply.id, &p.id)) {
                Some(loaded) => Target::Found(Box::new(loaded.clone())),
                None => {
                    cmds.push(Cmd::LoadPost(reply.id.clone()));
                    Target::Asked
                }
            };
            self.replies.insert(reply.id, target);
        }
        cmds
    }

    /// The node's answer about `reference`: the post, `None` when it has no
    /// such post, or why it could not be asked. Only a question still open
    /// takes it: what arrived or was deleted meanwhile is newer.
    pub(crate) fn on_reply_target(
        &mut self,
        reference: String,
        post: Result<Option<Box<PostJson>>, String>,
    ) {
        if self.replies.get(&reference) != Some(&Target::Asked) {
            return;
        }
        let target = match post {
            Ok(Some(post)) => Target::Found(Box::new(convert::post_item(&post))),
            Ok(None) => Target::Missing,
            Err(_) => Target::Failed,
        };
        self.keeping_view(|app| {
            app.replies.insert(reference, target);
        });
    }

    /// `post` came in live, maybe in another channel: replies still waiting
    /// on it, or told it was missing (it had not synced here yet), have it.
    pub(crate) fn reply_target_arrived(&mut self, post: &PostItem) {
        let waiting = |(reference, target): (&String, &Target)| {
            names(reference, &post.id) && !matches!(target, Target::Found(_))
        };
        if !self.replies.iter().any(waiting) {
            return;
        }
        self.keeping_view(|app| {
            for (reference, target) in app.replies.iter_mut() {
                if waiting((reference, &*target)) {
                    *target = Target::Found(Box::new(post.clone()));
                }
            }
        });
    }

    /// Post `id` was deleted: the replies naming it say it is not here,
    /// without asking the node. (Every id the view's replies name is in
    /// [`App::replies`] by now, loaded targets too.)
    pub(crate) fn reply_target_deleted(&mut self, id: &str) {
        for (reference, target) in self.replies.iter_mut() {
            if names(reference, id) {
                *target = Target::Missing;
            }
        }
    }

    /// Channel `name` was deleted, and its posts are no longer shown: what
    /// replies knew of them is asked for again.
    pub(crate) fn reply_targets_lost_channel(&mut self, name: &str) {
        self.replies
            .retain(|_, target| !matches!(target, Target::Found(post) if post.channel == name));
    }

    /// The live feed is back: ids the node could not be asked about, or
    /// did not have (they may have synced meanwhile), are asked again.
    pub(crate) fn retry_reply_targets(&mut self) {
        self.replies
            .retain(|_, target| matches!(target, Target::Found(_) | Target::Asked));
    }

    /// Makes `change` (to what replies resolve to) without moving a view
    /// scrolled up: the posts below it that grow or shrink (a reply line
    /// resolving, its body losing `re <id>:`) move the scroll with them, as
    /// a post arriving there does.
    fn keeping_view(&mut self, change: impl FnOnce(&mut App)) {
        let width = match self.viewport() {
            Some(view) if self.scroll > 0 => view.width,
            _ => return change(self),
        };
        let before = posts::spans(self, width);
        change(self);
        let after = posts::spans(self, width);
        let mut scroll = self.scroll;
        // Newest first: the posts below the view come first.
        for (old, new) in before.iter().zip(&after) {
            if old.bottom >= self.scroll {
                break;
            }
            scroll = (scroll + new.height).saturating_sub(old.height);
        }
        self.scroll = scroll;
        self.clamp_scroll();
    }
}
